//! Order-preserving parallel map over a small thread pool, for read + hash + parse.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Number of worker threads used for file reading and parsing.
pub fn workers() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 4)
}

/// Apply `f` to every item on up to `workers()` scoped threads; results keep input order.
pub fn map<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let n = items.len();
    let threads = workers().min(n);
    if threads <= 1 {
        return items.into_iter().map(f).collect();
    }
    let inputs: Vec<Mutex<Option<T>>> = items.into_iter().map(|t| Mutex::new(Some(t))).collect();
    let outputs: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let item = inputs[i].lock().unwrap().take().unwrap();
                    let r = f(item);
                    *outputs[i].lock().unwrap() = Some(r);
                }
            });
        }
    });
    outputs
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect()
}
