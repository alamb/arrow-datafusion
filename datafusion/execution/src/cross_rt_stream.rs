// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! [CrossRtStream] runs [`Stream`]s in a different tokio runtime.

//! Tooling to pull [`Stream`]s from one tokio runtime into another.
//!
//! Originally from [InfluxDB 3.0]
//! [InfluxDB 3.0]:https://github.com/influxdata/influxdb3_core/blob/6fcbb004232738d55655f32f4ad2385523d10696/iox_query/src/exec/cross_rt_stream.rs#L1
//!
//! This is critical so that CPU heavy loads are not run on the same runtime as IO handling

// TODO: figure out where ot pull this code (not in physical plan...)
// maybe its own crate or maybe in common-runtime ??

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use crate::dedicated_executor::{DedicatedExecutor, JobError};
use datafusion_common::DataFusionError;
use futures::{future::BoxFuture, ready, FutureExt, Stream, StreamExt};
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;

/// [`Stream`] that is calculated by one tokio runtime but can safely be pulled
/// from another w/o stalling (esp. when the calculating runtime is
/// CPU-blocked).
///
/// See XXX in the architecture documentation for moe details
pub struct CrossRtStream<T> {
    /// Future that drives the underlying stream.
    ///
    /// This is actually wrapped into [`DedicatedExecutor::spawn_cpu`] so it can be safely polled by the receiving runtime.
    driver: BoxFuture<'static, ()>,

    /// Flags if the [driver](Self::driver) returned [`Poll::Ready`].
    driver_ready: bool,

    /// Receiving stream.
    ///
    /// This one can be polled from the receiving runtime.
    inner: ReceiverStream<T>,

    /// Signals that [`inner`](Self::inner) finished.
    ///
    /// Note that we must also drive the [driver](Self::driver) even when the stream finished to allow proper state clean-ups.
    inner_done: bool,
}

impl<T> std::fmt::Debug for CrossRtStream<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossRtStream")
            .field("driver", &"...")
            .field("driver_ready", &self.driver_ready)
            .field("inner", &"...")
            .field("inner_done", &self.inner_done)
            .finish()
    }
}

impl<T> CrossRtStream<T> {
    /// Create new stream by producing a future that sends its state to the given [`Sender`].
    ///
    /// This is an internal method. `f` should always be wrapped into [`DedicatedExecutor::spawn_cpu`] (except for testing purposes).
    fn new_with_tx<F, Fut>(f: F) -> Self
    where
        F: FnOnce(Sender<T>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (tx, rx) = channel(1);
        let driver = f(tx).boxed();
        Self {
            driver,
            driver_ready: false,
            inner: ReceiverStream::new(rx),
            inner_done: false,
        }
    }
}

impl<X, E> CrossRtStream<Result<X, E>>
where
    X: Send + 'static,
    E: Send + 'static,
{
    /// Create new stream based on an existing stream that transports [`Result`]s.
    ///
    /// Also receives an executor that actually executes the underlying stream as well as a converter that converts
    /// [`executor::JobError`] to the error type of the stream (so we can send potential crashes/panics).
    pub fn new_with_error_stream<S, C>(
        stream: S,
        exec: DedicatedExecutor,
        converter: C,
    ) -> Self
    where
        S: Stream<Item = Result<X, E>> + Send + 'static,
        C: Fn(JobError) -> E + Send + 'static,
    {
        Self::new_with_tx(|tx| {
            // future to be run in the other runtime
            let tx_captured = tx.clone();
            let fut = async move {
                tokio::pin!(stream);

                while let Some(res) = stream.next().await {
                    if tx_captured.send(res).await.is_err() {
                        // receiver gone
                        return;
                    }
                }
            };

            // future for this runtime (likely the tokio/tonic/web driver)
            async move {
                if let Err(e) = exec.spawn_cpu(fut).await {
                    let e = converter(e);

                    // last message, so we don't care about the receiver side
                    tx.send(Err(e)).await.ok();
                }
            }
        })
    }
}

impl<X> CrossRtStream<Result<X, DataFusionError>>
where
    X: Send + 'static,
{
    /// Create new stream based on an existing stream that transports [`Result`]s w/ [`DataFusionError`]s.
    ///
    /// Also receives an executor that actually executes the underlying stream.
    pub fn new_with_df_error_stream<S>(stream: S, exec: DedicatedExecutor) -> Self
    where
        S: Stream<Item = Result<X, DataFusionError>> + Send + 'static,
    {
        Self::new_with_error_stream(stream, exec, |e| {
            DataFusionError::Context(
                "Join Error (panic)".to_string(),
                Box::new(DataFusionError::External(e.into())),
            )
        })
    }
}

impl<T> Stream for CrossRtStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = &mut *self;

        if !this.driver_ready {
            let res = this.driver.poll_unpin(cx);

            if res.is_ready() {
                this.driver_ready = true;
            }
        }

        if this.inner_done {
            if this.driver_ready {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        } else {
            match ready!(this.inner.poll_next_unpin(cx)) {
                None => {
                    this.inner_done = true;
                    if this.driver_ready {
                        Poll::Ready(None)
                    } else {
                        Poll::Pending
                    }
                }
                Some(x) => Poll::Ready(Some(x)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedicated_executor::DedicatedExecutorBuilder;
    use std::sync::OnceLock;
    use std::{sync::Arc, time::Duration};
    use tokio::runtime::{Handle, RuntimeFlavor};

    // Don't create many different runtimes for testing to avoid thread creation/description overhead
    fn testing_executor() -> DedicatedExecutor {
        TESTING_EXECUTOR
            .get_or_init(|| {
                DedicatedExecutorBuilder::new()
                    .with_name("cross_rt_stream")
                    .build()
            })
            .clone()
    }
    static TESTING_EXECUTOR: OnceLock<DedicatedExecutor> = OnceLock::new();

    #[tokio::test]
    async fn test_async_block() {
        let exec = testing_executor();
        let barrier1 = Arc::new(tokio::sync::Barrier::new(2));
        let barrier1_captured = Arc::clone(&barrier1);
        let barrier2 = Arc::new(tokio::sync::Barrier::new(2));
        let barrier2_captured = Arc::clone(&barrier2);
        let mut stream = CrossRtStream::<Result<u8, JobError>>::new_with_error_stream(
            futures::stream::once(async move {
                barrier1_captured.wait().await;
                barrier2_captured.wait().await;
                Ok(1)
            }),
            exec,
            std::convert::identity,
        );

        let mut f = stream.next();

        ensure_pending(&mut f).await;
        barrier1.wait().await;
        ensure_pending(&mut f).await;
        barrier2.wait().await;

        let res = f.await.expect("streamed data");
        assert_eq!(res.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_sync_block() {
        // This would deadlock if the stream payload would run within the same tokio runtime. To prevent any cheating
        // (e.g. via channels), we ensure that the current runtime only has a single thread:
        assert_eq!(
            RuntimeFlavor::CurrentThread,
            Handle::current().runtime_flavor()
        );

        let exec = testing_executor();
        let barrier1 = Arc::new(std::sync::Barrier::new(2));
        let barrier1_captured = Arc::clone(&barrier1);
        let barrier2 = Arc::new(std::sync::Barrier::new(2));
        let barrier2_captured = Arc::clone(&barrier2);
        let mut stream = CrossRtStream::<Result<u8, JobError>>::new_with_error_stream(
            futures::stream::once(async move {
                barrier1_captured.wait();
                barrier2_captured.wait();
                Ok(1)
            }),
            exec,
            std::convert::identity,
        );

        let mut f = stream.next();

        ensure_pending(&mut f).await;
        barrier1.wait();
        ensure_pending(&mut f).await;
        barrier2.wait();

        let res = f.await.expect("streamed data");
        assert_eq!(res.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_panic() {
        let exec = testing_executor();
        let mut stream = CrossRtStream::<Result<(), JobError>>::new_with_error_stream(
            futures::stream::once(async { panic!("foo") }),
            exec,
            std::convert::identity,
        );

        let e = stream
            .next()
            .await
            .expect("stream not finished")
            .unwrap_err();
        assert_eq!(e.to_string(), "Panic: foo");

        let none = stream.next().await;
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn test_cancel_future() {
        let exec = testing_executor;
        let barrier1 = Arc::new(tokio::sync::Barrier::new(2));
        let barrier1_captured = Arc::clone(&barrier1);
        let barrier2 = Arc::new(tokio::sync::Barrier::new(2));
        let barrier2_captured = Arc::clone(&barrier2);
        let mut stream = CrossRtStream::<Result<u8, JobError>>::new_with_error_stream(
            futures::stream::once(async move {
                barrier1_captured.wait().await;
                barrier2_captured.wait().await;
                Ok(1)
            }),
            exec,
            std::convert::identity,
        );

        let mut f = stream.next();

        // fire up stream
        ensure_pending(&mut f).await;
        barrier1.wait().await;

        // cancel
        drop(f);

        barrier2.wait().await;
        let res = stream.next().await.expect("streamed data");
        assert_eq!(res.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_cancel_stream() {
        let exec = testing_executor();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier_captured = Arc::clone(&barrier);
        let mut stream = CrossRtStream::<Result<u8, JobError>>::new_with_error_stream(
            futures::stream::once(async move {
                barrier_captured.wait().await;

                // block forever
                futures::future::pending::<()>().await;

                // keep barrier Arc alive
                drop(barrier_captured);
                unreachable!()
            }),
            exec,
            std::convert::identity,
        );

        let mut f = stream.next();

        // fire up stream
        ensure_pending(&mut f).await;
        barrier.wait().await;
        assert_eq!(Arc::strong_count(&barrier), 2);

        // cancel
        drop(f);
        drop(stream);

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if Arc::strong_count(&barrier) == 1 {
                    return;
                }

                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_inner_future_driven_to_completion_after_stream_ready() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier_captured = Arc::clone(&barrier);

        let mut stream = CrossRtStream::<u8>::new_with_tx(|tx| async move {
            tx.send(1).await.ok();
            drop(tx);
            barrier_captured.wait().await;
        });

        let handle = tokio::spawn(async move { barrier.wait().await });

        assert_eq!(stream.next().await, Some(1));
        handle.await.unwrap();
    }

    async fn ensure_pending<F>(f: &mut F)
    where
        F: Future + Send + Unpin,
    {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            _ = f => {panic!("not pending")},
        }
    }
}
