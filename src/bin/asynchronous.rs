use runtime::*;
use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
};

fn main() {
    enum Main {
        Start,
        Accept { listener: TcpListener },
    }

    impl Future for Main {
        type Output = ();

        fn poll(&mut self, waker: Waker) -> Option<()> {
            if let Main::Start = self {
                let listener = TcpListener::bind("localhost:3000").unwrap();
                listener.set_nonblocking(true).unwrap();

                REACTOR.with(|reactor| {
                    reactor.add(listener.as_raw_fd(), waker);
                });

                *self = Main::Accept { listener };
            }

            if let Main::Accept { listener } = self {
                match listener.accept() {
                    Ok((connection, _)) => {
                        connection.set_nonblocking(true).unwrap();
                        SCHEDULER.spawn(Handler {
                            connection,
                            state: HandlerState::Start,
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return None;
                    }
                    Err(e) => panic!("{e}"),
                }
            }

            None
        }
    }

    SCHEDULER.spawn(Main::Start);
    SCHEDULER.run();
}

struct Handler {
    connection: TcpStream,
    state: HandlerState,
}

enum HandlerState {
    Start,
    Read {
        request: [u8; 1024],
        read: usize,
    },
    Write {
        response: &'static [u8],
        written: usize,
    },
    Flush,
}

impl Future for Handler {
    type Output = ();

    fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
        if let HandlerState::Start = self.state {
            // start by registering our connection for notifications
            REACTOR.with(|reactor| {
                reactor.add(self.connection.as_raw_fd(), waker);
            });

            self.state = HandlerState::Read {
                request: [0u8; 1024],
                read: 0,
            };
        }

        if let HandlerState::Read { request, read } = &mut self.state {
            loop {
                match self.connection.read(request) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => *read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                // did we reach the end of the request?
                let read = *read;
                if read >= 4 && &request[read - 4..read] == b"\r\n\r\n" {
                    break;
                }
            }

            // we're done, print the request
            let request = String::from_utf8_lossy(&request[..*read]);
            println!("{}", request);

            // and move into the write state
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "Content-Length: 12\n",
                "Connection: close\r\n\r\n",
                "Hello world!"
            );

            self.state = HandlerState::Write {
                response: response.as_bytes(),
                written: 0,
            };
        }

        // write the response
        if let HandlerState::Write { response, written } = &mut self.state {
            loop {
                match self.connection.write(&response[*written..]) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => *written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                // did we write the whole response yet?
                if *written == response.len() {
                    break;
                }
            }

            // successfully wrote the response, try flushing next
            self.state = HandlerState::Flush;
        }

        // flush the response
        if let HandlerState::Flush = self.state {
            match self.connection.flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                Err(e) => panic!("{e}"),
            }
        }

        REACTOR.with(|reactor| {
            reactor.remove(self.connection.as_raw_fd());
        });

        Some(())
    }
}
mod runtime {
    use std::{
        cell::RefCell,
        collections::{HashMap, VecDeque},
        os::fd::RawFd,
        sync::{Arc, Mutex},
    };

    use epoll::Events;
    use epoll::{ControlOptions::EPOLL_CTL_ADD, Event};

    #[derive(Clone)]
    pub struct Waker(Arc<dyn Fn() + Send + Sync>);

    impl Waker {
        pub fn wake(&self) {
            (self.0)()
        }
    }

    pub trait Future {
        type Output;

        fn poll(&mut self, waker: Waker) -> Option<Self::Output>;
    }

    pub static SCHEDULER: Scheduler = Scheduler {
        runnable: Mutex::new(VecDeque::new()),
    };

    // The scheduler.
    #[derive(Default)]
    pub struct Scheduler {
        runnable: Mutex<VecDeque<SharedTask>>,
    }

    pub type SharedTask = Arc<Mutex<dyn Future<Output = ()> + Send>>;

    impl Scheduler {
        pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
            self.runnable
                .lock()
                .unwrap()
                .push_back(Arc::new(Mutex::new(task)));
        }

        pub fn run(&self) {
            loop {
                loop {
                    // pop a runnable task off the queue
                    let Some(task) = self.runnable.lock().unwrap().pop_front() else { break };
                    let t2 = task.clone();

                    // create a waker that pushes the task back on
                    let wake = Arc::new(move || {
                        SCHEDULER.runnable.lock().unwrap().push_back(t2.clone());
                    });

                    // poll the task
                    task.lock().unwrap().poll(Waker(wake));
                }

                // if there are no runnable tasks, block on epoll until something becomes ready
                REACTOR.with(|reactor| reactor.wait());
            }
        }
    }

    thread_local! {
        pub static REACTOR: Reactor = Reactor::new();
    }

    // The reactor.
    pub struct Reactor {
        epoll: RawFd,
        tasks: RefCell<HashMap<RawFd, Waker>>,
    }

    impl Reactor {
        pub fn new() -> Reactor {
            Reactor {
                epoll: epoll::create(false).unwrap(),
                tasks: RefCell::new(HashMap::new()),
            }
        }

        pub fn add(&self, fd: RawFd, waker: Waker) {
            let event = epoll::Event::new(Events::EPOLLIN | Events::EPOLLOUT, fd as u64);
            epoll::ctl(self.epoll, EPOLL_CTL_ADD, fd, event).unwrap();
            self.tasks.borrow_mut().insert(fd, waker);
        }

        pub fn remove(&self, fd: RawFd) {
            self.tasks.borrow_mut().remove(&fd);
        }

        pub fn wait(&self) {
            let mut events = [Event::new(Events::empty(), 0); 1024];
            let timeout = -1; // forever
            let num_events = epoll::wait(self.epoll, timeout, &mut events).unwrap();

            for event in &events[..num_events] {
                let fd = event.data as i32;
                let tasks = self.tasks.borrow_mut();

                // notify the task
                if let Some(waker) = tasks.get(&fd) {
                    waker.wake();
                }
            }
        }
    }
}
