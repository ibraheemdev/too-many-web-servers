use std::{
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::AsRawFd,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use signal_hook::{consts::SIGINT, iterator::Signals};

use runtime::*;

fn spawn_blocking(blocking_work: impl FnOnce() + Send + 'static) -> impl Future<Output = ()> {
    let state: Arc<Mutex<(bool, Option<Waker>)>> = Arc::default();
    let state_handle = state.clone();

    // run the blocking work on a separate thread
    std::thread::spawn(move || {
        // run the work
        blocking_work();

        let (done, waker) = &mut *state_handle.lock().unwrap();

        // mark the task as done
        *done = true;

        // wake the waker
        if let Some(waker) = waker.take() {
            waker.wake();
        }
    });

    poll_fn(move |waker| match &mut *state.lock().unwrap() {
        // work is not completed, store our waker and come back later
        (false, state) => {
            *state = Some(waker);
            None
        }
        // the work is completed
        (true, _) => Some(()),
    })
}

pub fn main() {
    SCHEDULER.spawn(listen());
    SCHEDULER.run();
}

fn ctrl_c() -> impl Future<Output = ()> {
    spawn_blocking(|| {
        let mut signal = Signals::new(&[SIGINT]).unwrap();
        let _ctrl_c = signal.forever().next().unwrap();
    })
}

fn listen() -> impl Future<Output = ()> {
    let tasks = Arc::new(Counter::default());
    let tasks_ref = tasks.clone();

    poll_fn(|waker| {
        let listener = TcpListener::bind("localhost:3000").unwrap();

        listener.set_nonblocking(true).unwrap();

        REACTOR.with(|reactor| {
            reactor.add(listener.as_raw_fd(), waker);
        });

        Some(listener)
    })
    .chain(|listener| {
        let listen = poll_fn(move |_| match listener.accept() {
            Ok((connection, _)) => {
                connection.set_nonblocking(true).unwrap();

                tasks.increment();

                let tasks = tasks.clone();
                SCHEDULER.spawn(handle(connection).chain(|_| {
                    poll_fn(move |_| {
                        tasks.decrement();
                        Some(())
                    })
                }));
                None::<()>
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => None,
            Err(e) => panic!("{e}"),
        });

        select(listen, ctrl_c())
    })
    .chain(|_ctrl_c| graceful_shutdown(tasks_ref))
}

fn graceful_shutdown(tasks: Arc<Counter>) -> impl Future<Output = ()> {
    let timer = spawn_blocking(|| thread::sleep(Duration::from_secs(30)));
    let request_counter = tasks.wait_for_zero();

    select(timer, request_counter).chain(|_| {
        poll_fn(|_| {
            // graceful shutdown process complete, now we actually exit
            println!("Graceful shutdown complete");
            std::process::exit(0)
        })
    })
}

#[derive(Default)]
struct Counter {
    state: Mutex<(usize, Option<Waker>)>,
}

impl Counter {
    fn increment(&self) {
        let (count, _) = &mut *self.state.lock().unwrap();
        *count += 1;
    }

    fn decrement(&self) {
        let (count, waker) = &mut *self.state.lock().unwrap();
        *count -= 1;

        // we were the last task
        if *count == 0 {
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    }

    fn wait_for_zero(self: Arc<Self>) -> impl Future<Output = ()> {
        poll_fn(move |waker| {
            match &mut *self.state.lock().unwrap() {
                // work is completed
                (0, _) => Some(()),
                // work is not completed, store our waker and come back later
                (_, state) => {
                    *state = Some(waker);
                    None
                }
            }
        })
    }
}

fn handle(connection: TcpStream) -> impl Future<Output = ()> {
    let connection = Arc::new(connection);
    let read_connection_ref = connection.clone();
    let write_connection_ref = connection.clone();
    let flush_connection_ref = connection.clone();

    poll_fn(move |waker| {
        REACTOR.with(|reactor| {
            reactor.add(connection.as_raw_fd(), waker);
        });

        Some(())
    })
    .chain(move |_| {
        let mut request = [0u8; 1024];
        let mut read = 0;

        poll_fn(move |_| {
            let connection = &mut &*read_connection_ref;

            loop {
                // try reading from the stream
                match connection.read(&mut request) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => read += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                // did we reach the end of the request?
                let read = read;
                if read >= 4 && &request[read - 4..read] == b"\r\n\r\n" {
                    break;
                }
            }

            // we're done, print the request
            let request = String::from_utf8_lossy(&request[..read]);
            println!("{request}");

            Some(())
        })
    })
    .chain(move |_| {
        let response = concat!(
            "HTTP/1.1 200 OK\r\n",
            "Content-Length: 12\n",
            "Connection: close\r\n\r\n",
            "Hello world!"
        );
        let mut written = 0;

        poll_fn(move |_| {
            let connection = &mut &*write_connection_ref;

            loop {
                match connection.write(&response.as_bytes()[written..]) {
                    Ok(0) => {
                        println!("client disconnected unexpectedly");
                        return Some(());
                    }
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
                    Err(e) => panic!("{e}"),
                }

                // did we write the whole response yet?
                if written == response.len() {
                    break;
                }
            }

            Some(())
        })
    })
    .chain(move |_| {
        poll_fn(move |_| {
            let connection = &mut &*flush_connection_ref;

            match connection.flush() {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return None;
                }
                Err(e) => panic!("{e}"),
            };

            REACTOR.with(|reactor| {
                reactor.remove(connection.as_raw_fd());
            });

            Some(())
        })
    })
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

        // chain two futures together, running the second after the first completes
        fn chain<F, T>(self, transition: F) -> Chain<Self, F, T>
        where
            F: FnOnce(Self::Output) -> T,
            T: Future,
            Self: Sized,
        {
            Chain::First {
                future1: self,
                transition: Some(transition),
            }
        }
    }

    pub enum Chain<T1, F, T2> {
        First { future1: T1, transition: Option<F> },
        Second { future2: T2 },
    }

    impl<T1, F, T2> Future for Chain<T1, F, T2>
    where
        T1: Future,
        F: FnOnce(T1::Output) -> T2,
        T2: Future,
    {
        type Output = T2::Output;

        fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
            if let Chain::First {
                future1,
                transition,
            } = self
            {
                // poll the first future
                match future1.poll(waker.clone()) {
                    Some(value) => {
                        // first future is done, transition into the second
                        let future2 = (transition.take().unwrap())(value);
                        *self = Chain::Second { future2 };
                    }
                    // first future is not ready, return
                    None => return None,
                }
            }

            if let Chain::Second { future2 } = self {
                // first future is already done, poll the second
                return future2.poll(waker);
            }

            None
        }
    }

    // select between two futures, returning the output of whichever
    // one completes first
    pub fn select<L, R>(left: L, right: R) -> Select<L, R> {
        Select { left, right }
    }

    pub struct Select<L, R> {
        left: L,
        right: R,
    }

    pub enum Either<L, R> {
        Left(L),
        Right(R),
    }

    impl<L: Future, R: Future> Future for Select<L, R> {
        type Output = Either<L::Output, R::Output>;

        fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
            if let Some(output) = self.left.poll(waker.clone()) {
                return Some(Either::Left(output));
            }

            if let Some(output) = self.right.poll(waker) {
                return Some(Either::Right(output));
            }

            None
        }
    }

    // create a future with the given `poll` function
    pub fn poll_fn<F, T>(f: F) -> impl Future<Output = T>
    where
        F: FnMut(Waker) -> Option<T>,
    {
        struct PollFn<F>(F);

        impl<F, T> Future for PollFn<F>
        where
            F: FnMut(Waker) -> Option<T>,
        {
            type Output = T;

            fn poll(&mut self, waker: Waker) -> Option<Self::Output> {
                (self.0)(waker)
            }
        }

        PollFn(f)
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
