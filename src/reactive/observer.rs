//! Observer trait for the reactive layer.

use crate::error::PgmcpError;

/// Observer that receives values from an Observable.
pub trait Observer<T: Send>: Send {
    fn on_next(&self, value: T);
    fn on_error(&self, error: PgmcpError);
    fn on_complete(&self);
}

/// A simple function-based observer.
pub struct FnObserver<T> {
    on_next_fn: Box<dyn Fn(T) + Send>,
    on_error_fn: Box<dyn Fn(PgmcpError) + Send>,
    on_complete_fn: Box<dyn Fn() + Send>,
}

impl<T: Send> FnObserver<T> {
    pub fn new(
        on_next: impl Fn(T) + Send + 'static,
        on_error: impl Fn(PgmcpError) + Send + 'static,
        on_complete: impl Fn() + Send + 'static,
    ) -> Self {
        Self {
            on_next_fn: Box::new(on_next),
            on_error_fn: Box::new(on_error),
            on_complete_fn: Box::new(on_complete),
        }
    }
}

impl<T: Send> Observer<T> for FnObserver<T> {
    fn on_next(&self, value: T) {
        (self.on_next_fn)(value);
    }

    fn on_error(&self, error: PgmcpError) {
        (self.on_error_fn)(error);
    }

    fn on_complete(&self) {
        (self.on_complete_fn)();
    }
}
