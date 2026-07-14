//! Panic-safe mutex access shared by the black-box flush paths: never
//! blocks (the panicking thread may hold the lock) and recovers poisoned
//! mutexes instead of propagating the poison.

use std::sync::{Mutex, MutexGuard, TryLockError};

/// `try_lock` that treats a poisoned mutex as usable (`into_inner`) and a
/// held mutex (`WouldBlock`) as `None`.
pub(crate) fn try_lock_recover<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn locks_recovers_poison_and_skips_held() {
        let m = Arc::new(Mutex::new(1));
        assert_eq!(*try_lock_recover(&m).unwrap(), 1);

        // Poison it, then recover.
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(*try_lock_recover(&m).unwrap(), 1);

        // Held elsewhere → None.
        let g = try_lock_recover(&m).unwrap();
        assert!(try_lock_recover(&m).is_none());
        drop(g);
    }
}
