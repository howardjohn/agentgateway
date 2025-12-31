use crate::*;
use hyper::rt::Sleep;
use std::pin::Pin;
use std::time::{Duration, Instant};

pub struct PingoraTimer;
struct PingoraSleep(Pin<Box<dyn Future<Output = ()> + Send + Sync>>);

impl Future for PingoraSleep {
	type Output = ();

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
		match Pin::new(&mut self.0).poll(cx) {
			Poll::Ready(_) => Poll::Ready(()),
			Poll::Pending => Poll::Pending,
		}
	}
}
impl hyper::rt::Sleep for PingoraSleep {}

impl hyper::rt::Timer for PingoraTimer {
	fn sleep(&self, duration: Duration) -> Pin<Box<dyn Sleep>> {
		Box::pin(PingoraSleep(Box::pin(
			pingora_timeout::fast_timeout::fast_sleep(duration),
		)))
	}

	fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Sleep>> {
		self.sleep(deadline - std::time::Instant::now())
	}
}
