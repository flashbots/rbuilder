use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[derive(Clone)]
pub struct BlockCell<T> {
    inner: Arc<Mutex<BlockCellInner<T>>>,
}

struct BlockCellInner<T> {
    value: Option<T>,
    version: u64,
    last_polled_version: u64,
    waker: Option<Waker>,
}

impl<T: Clone> BlockCell<T> {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BlockCellInner {
                value: None,
                version: 0,
                last_polled_version: 0,
                waker: None,
            })),
        }
    }

    pub fn set(&self, value: Option<T>) {
        let mut inner = self.inner.lock().unwrap();
        inner.value = value;
        inner.version += 1;
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }

    pub fn get(&self) -> Option<T> {
        let inner = self.inner.lock().unwrap();
        inner.value.clone()
    }

    pub fn poll_updated(&self, cx: &Context<'_>) -> Poll<Option<T>> {
        let mut inner = self.inner.lock().unwrap();
        if inner.version > inner.last_polled_version {
            inner.last_polled_version = inner.version;
            Poll::Ready(inner.value.clone())
        } else {
            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::time::Duration;
    use tokio::time::timeout;

    struct WaitUpdate {
        cell: BlockCell<i32>,
    }

    impl WaitUpdate {
        fn new(cell: BlockCell<i32>) -> Self {
            Self { cell }
        }
    }

    impl Future for WaitUpdate {
        type Output = Option<i32>;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.cell.poll_updated(cx)
        }
    }

    #[tokio::test]
    async fn test_basic_usage() {
        let cell: BlockCell<i32> = BlockCell::new();
        assert_eq!(cell.get(), None);

        cell.set(Some(2));
        assert_eq!(cell.get(), Some(2));

        cell.set(None);
        assert_eq!(cell.get(), None);
    }

    #[tokio::test]
    async fn test_wait_for_update() {
        let cell = BlockCell::new();

        let cell_clone = cell.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cell_clone.set(Some(2));
        });

        let wait_future = WaitUpdate::new(cell.clone());
        let result = timeout(Duration::from_millis(200), wait_future).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn test_multiple_waiters() {
        let cell = BlockCell::new();

        let wait1 = WaitUpdate::new(cell.clone());
        let wait2 = WaitUpdate::new(cell.clone());

        let cell_clone = cell.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cell_clone.set(Some(2));
        });

        let (result1, result2) = tokio::join!(wait1, wait2);
        assert_eq!(result1, Some(2));
        assert_eq!(result2, Some(2));
    }

    #[tokio::test]
    async fn test_none_update() {
        let cell = BlockCell::new();

        cell.set(Some(1));
        assert_eq!(cell.get(), Some(1));

        let cell_clone = cell.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cell_clone.set(None);
        });

        let wait_future = WaitUpdate::new(cell.clone());
        let result = timeout(Duration::from_millis(200), wait_future).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }
}
