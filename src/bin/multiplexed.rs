use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::TcpListener,
    os::fd::AsRawFd,
};

use epoll::{ControlOptions::*, Event, Events};

enum ConnectionState {
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

pub fn main() {
    let listener = TcpListener::bind("localhost:3000").unwrap();
    listener.set_nonblocking(true).unwrap();

    let epoll = epoll::create(false).unwrap();

    // add the listener to epoll
    let event = Event::new(Events::EPOLLIN, listener.as_raw_fd() as _);
    epoll::ctl(epoll, EPOLL_CTL_ADD, listener.as_raw_fd(), event).unwrap();

    let mut connections = HashMap::new();

    loop {
        let mut events = [Event::new(Events::empty(), 0); 1024];
        let timeout = -1; // block forever, until something happens
        let num_events = epoll::wait(epoll, timeout, &mut events).unwrap();

        let mut completed = Vec::new();
        'next: for event in &events[..num_events] {
            let fd = event.data as i32;

            // is the listener ready?
            if fd == listener.as_raw_fd() {
                // try accepting a connection
                match listener.accept() {
                    Ok((connection, _)) => {
                        connection.set_nonblocking(true).unwrap();

                        let fd = connection.as_raw_fd();

                        // register the connection with epoll
                        let event = Event::new(Events::EPOLLIN | Events::EPOLLOUT, fd as _);
                        epoll::ctl(epoll, EPOLL_CTL_ADD, fd, event).unwrap();

                        let state = ConnectionState::Read {
                            request: [0u8; 1024],
                            read: 0,
                        };

                        connections.insert(fd, (connection, state));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) => panic!("failed to accept: {err}"),
                }

                continue 'next;
            }

            // otherwise, a connection must be ready
            let (connection, state) = connections.get_mut(&fd).unwrap();

            if let ConnectionState::Read { request, read } = state {
                loop {
                    // try reading from the stream
                    match connection.read(&mut *request) {
                        Ok(0) => {
                            println!("client disconnected unexpectedly");
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => {
                            // keep track of how many bytes we've read
                            *read += n;
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            // not ready yet, move on to the next connection
                            continue 'next;
                        }
                        Err(err) => panic!("{err}"),
                    }

                    // did we reach the end of the request?
                    if request.get(*read - 4..*read) == Some(b"\r\n\r\n") {
                        break;
                    }
                }

                // we're done, print the request
                let request = String::from_utf8_lossy(&request[..*read]);
                println!("{request}");
                // move into the write state
                let response = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Length: 12\n",
                    "Connection: close\r\n\r\n",
                    "Hello world!"
                );

                *state = ConnectionState::Write {
                    response: response.as_bytes(),
                    written: 0,
                };
            }

            if let ConnectionState::Write { response, written } = state {
                loop {
                    match connection.write(&response[*written..]) {
                        Ok(0) => {
                            // client disconnected, mark this connection as complete
                            completed.push(fd);
                            continue 'next;
                        }
                        Ok(n) => {
                            *written += n;
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            // not ready yet, move on to the next connection
                            continue 'next;
                        }
                        Err(e) => panic!("{e}"),
                    }

                    // did we write the whole response yet?
                    if *written == response.len() {
                        break;
                    }
                }

                // successfully wrote the response, try flushing next
                *state = ConnectionState::Flush;
            }

            if let ConnectionState::Flush = state {
                match connection.flush() {
                    Ok(_) => {
                        completed.push(fd);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // not ready yet, move on to the next connection
                        continue 'next;
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }

        for fd in completed {
            let (connection, _state) = connections.remove(&fd).unwrap();
            // unregister from epoll
            drop(connection);
        }
    }
}
