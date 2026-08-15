use std::fmt::{self, Display, Formatter};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    number: i32,
}

impl Signal {
    pub const fn new(number: i32) -> Self {
        Self { number }
    }

    pub const fn number(self) -> i32 {
        self.number
    }
}

impl Display for Signal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "signal({})", self.number)
    }
}

pub trait SignalHandler: Send + Sync {
    fn handle(&self, signal: Signal);
}

#[cfg(unix)]
struct RegisteredSignal<H> {
    signal: Signal,
    receiver: tokio::signal::unix::Signal,
    handler: H,
}

pub struct SignalHandlers<H> {
    #[cfg(unix)]
    registered: Vec<RegisteredSignal<H>>,
    #[cfg(not(unix))]
    marker: std::marker::PhantomData<H>,
}

impl<H> Default for SignalHandlers<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H> SignalHandlers<H> {
    pub const fn new() -> Self {
        Self {
            #[cfg(unix)]
            registered: Vec::new(),
            #[cfg(not(unix))]
            marker: std::marker::PhantomData,
        }
    }

    #[cfg(unix)]
    pub fn register(&mut self, signal: Signal, handler: H) -> io::Result<&mut Self> {
        use tokio::signal::unix::{SignalKind, signal as listen};

        if self
            .registered
            .iter()
            .any(|registered| registered.signal == signal)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{signal} already has a handler"),
            ));
        }

        let receiver = listen(SignalKind::from_raw(signal.number()))?;
        self.registered.push(RegisteredSignal {
            signal,
            receiver,
            handler,
        });
        Ok(self)
    }

    #[cfg(not(unix))]
    pub fn register(&mut self, _signal: Signal, _handler: H) -> io::Result<&mut Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "raw signal registration is not supported on this platform",
        ))
    }
}

impl<H> SignalHandlers<H>
where
    H: SignalHandler,
{
    #[cfg(unix)]
    pub async fn run(mut self) -> io::Result<Signal> {
        use std::future::poll_fn;
        use std::task::Poll;

        let (index, signal) = poll_fn(|context| {
            for (index, registered) in self.registered.iter_mut().enumerate() {
                match registered.receiver.poll_recv(context) {
                    Poll::Ready(Some(())) => return Poll::Ready(Ok((index, registered.signal))),
                    Poll::Ready(None) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("{} listener closed", registered.signal),
                        )));
                    }
                    Poll::Pending => {}
                }
            }
            Poll::Pending
        })
        .await?;

        self.registered[index].handler.handle(signal);
        Ok(signal)
    }

    #[cfg(not(unix))]
    pub async fn run(self) -> io::Result<Signal> {
        std::future::pending().await
    }
}
