//! Multiple producers wake the UI once per batch. The consumer applies inputs outside the lock.
use std::{
    mem,
    sync::{Arc, Mutex},
};

pub struct Inbox<T> {
    shared: Arc<Queue<T>>,
}
struct Queue<T> {
    items: Mutex<Vec<T>>,
    wake: Box<dyn Fn() + Send + Sync>,
}
impl<T> Clone for Inbox<T> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}
impl<T> Inbox<T> {
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            shared: Arc::new(Queue {
                items: Mutex::default(),
                wake: Box::new(wake),
            }),
        }
    }
    pub fn push(&self, item: T) {
        let mut items = self.shared.items.lock().unwrap();
        let needs_wake = items.is_empty();
        items.push(item);
        drop(items);
        if needs_wake {
            (self.shared.wake)();
        }
    }
    pub fn starts_with(&self, predicate: impl FnOnce(&T) -> bool) -> bool {
        self.shared
            .items
            .lock()
            .unwrap()
            .first()
            .is_some_and(predicate)
    }
    pub fn take(&self) -> Vec<T> {
        mem::take(&mut self.shared.items.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn wakes_once_until_drained() {
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let inbox = Inbox::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
        });
        inbox.push(1);
        inbox.clone().push(2);
        assert_eq!(count.load(Ordering::Relaxed), 1);
        assert_eq!(inbox.take(), [1, 2]);
        inbox.push(3);
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }
}
