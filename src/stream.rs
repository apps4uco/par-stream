use crate::{
    common::*,
    config::{IntoParStreamParams, ParStreamParams},
    rt,
    utils::{self, TokioMpscReceiverExt as _},
};
use tokio::sync::{mpsc, oneshot, Mutex};

/// An extension trait that controls ordering of stream items.
pub trait StreamExt
where
    Self: Stream,
{
    /// Reorder the input items paired with a iteration count.
    ///
    /// The combinator asserts the input item has tuple type `(usize, T)`.
    /// It reorders the items according to the first value of input tuple.
    ///
    /// It is usually combined with [IndexedStreamExt::wrapping_enumerate], then
    /// applies a series of unordered parallel mapping, and finally reorders the values
    /// back by this method. It avoids reordering the values after each parallel mapping step.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     let doubled = stream::iter(0..1000)
    ///         // add enumerated index that does not panic on overflow
    ///         .enumerate()
    ///         // double the values in parallel
    ///         .par_then_unordered(None, move |(index, value)| {
    ///             // the closure is sent to parallel worker
    ///             async move { (index, value * 2) }
    ///         })
    ///         // add values by one in parallel
    ///         .par_then_unordered(None, move |(index, value)| {
    ///             // the closure is sent to parallel worker
    ///             async move { (index, value + 1) }
    ///         })
    ///         // reorder the values by enumerated index
    ///         .reorder_enumerated()
    ///         .collect::<Vec<_>>()
    ///         .await;
    ///     let expect = (0..1000).map(|value| value * 2 + 1).collect::<Vec<_>>();
    ///     assert_eq!(doubled, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn reorder_enumerated<T>(self) -> ReorderEnumerated<Self, T>
    where
        Self: Stream<Item = (usize, T)>;
}

impl<S> StreamExt for S
where
    S: Stream,
{
    fn reorder_enumerated<T>(self) -> ReorderEnumerated<Self, T>
    where
        Self: Stream<Item = (usize, T)>,
    {
        ReorderEnumerated {
            stream: self,
            commit: 0,
            buffer: HashMap::new(),
        }
    }
}

/// An extension trait that provides parallel processing combinators on streams.
pub trait ParStreamExt
where
    Self: 'static + Send + Stream + StreamExt,
    Self::Item: 'static + Send,
{
    fn scan_spawned<B, T, F, Fut>(
        self,
        buf_size: impl Into<Option<usize>>,
        init: B,
        map_fn: F,
    ) -> BoxStream<'static, T>
    where
        B: 'static + Send,
        T: 'static + Send,
        F: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: Future<Output = Option<(B, T)>> + Send;

    /// Maps the stream element to a different type on a spawned worker.
    fn then_spawned<T, F, Fut>(
        self,
        buf_size: impl Into<Option<usize>>,
        f: F,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: Future<Output = T> + Send;

    /// Maps the stream element to a different type on a parallel thread.
    fn map_spawned<T, F>(self, buf_size: impl Into<Option<usize>>, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> T + Send;

    /// A combinator that consumes as many elements as it likes, and produces the next stream element.
    ///
    /// The function f([receiver](flume::Receiver), [sender](flume::Sender)) takes one or more elements
    /// by calling `receiver.recv().await`,
    /// It returns `Some(item)` if an input element is available, otherwise it returns `None`.
    /// Calling `sender.send(item).await` will produce an output element. It returns `Ok(())` when success,
    /// or returns `Err(item)` if the output stream is closed.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    /// use std::mem;
    ///
    /// async fn main_async() {
    ///     let data = vec![1, 2, -3, 4, 5, -6, 7, 8];
    ///     let mut stream = stream::iter(data).batching(|mut rx, mut tx| async move {
    ///         let mut buffer = vec![];
    ///         while let Ok(value) = rx.recv_async().await {
    ///             buffer.push(value);
    ///             if value < 0 {
    ///                 let result = tx.send_async(mem::take(&mut buffer)).await;
    ///                 if result.is_err() {
    ///                     return;
    ///                 }
    ///             }
    ///         }
    ///
    ///         let _ = tx.send_async(mem::take(&mut buffer)).await;
    ///     });
    ///
    ///     assert_eq!(stream.next().await, Some(vec![1, 2, -3]));
    ///     assert_eq!(stream.next().await, Some(vec![4, 5, -6]));
    ///     assert_eq!(stream.next().await, Some(vec![7, 8]));
    ///     assert!(stream.next().await.is_none());
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn batching<T, F, Fut>(self, f: F) -> BoxStream<'static, T>
    where
        F: FnOnce(flume::Receiver<Self::Item>, flume::Sender<T>) -> Fut,
        Fut: 'static + Future<Output = ()> + Send,
        T: 'static + Send;

    /// The combinator maintains a collection of concurrent workers, each consuming as many elements as it likes,
    /// and produces the next stream element.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    /// use std::mem;
    ///
    /// async fn main_async() {
    ///     let data = vec![1, 2, -3, 4, 5, -6, 7, 8];
    ///     stream::iter(data).batching(|mut rx, mut tx| async move {
    ///         while let Ok(value) = rx.recv_async().await {
    ///             if value > 0 {
    ///                 let result = tx.send_async(value).await;
    ///                 if result.is_err() {
    ///                     return;
    ///                 }
    ///             }
    ///         }
    ///     });
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_batching_unordered<P, T, F, Fut>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        F: FnMut(usize, flume::Receiver<Self::Item>, flume::Sender<T>) -> Fut,
        Fut: 'static + Future<Output = ()> + Send,
        T: 'static + Send,
        P: IntoParStreamParams;

    /// Converts the stream to a cloneable receiver that receiving items in fan-out pattern.
    ///
    /// When a receiver is cloned, it creates a separate internal buffer, so that a background
    /// worker clones and passes each stream item to available receiver buffers. It can be used
    /// to _fork_ a stream into copies and pass them to concurrent workers.
    ///
    /// Each receiver maintains an internal buffer with `buf_size`. If one of the receiver buffer
    /// is full, the stream will halt until the blocking buffer spare the space.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     let orig: Vec<_> = (0..1000).collect();
    ///
    ///     let rx1 = stream::iter(orig.clone()).tee(1);
    ///     let rx2 = rx1.clone();
    ///     let rx3 = rx1.clone();
    ///
    ///     let fut1 = rx1.map(|val| val).collect();
    ///     let fut2 = rx2.map(|val| val * 2).collect();
    ///     let fut3 = rx3.map(|val| val * 3).collect();
    ///
    ///     let (vec1, vec2, vec3): (Vec<_>, Vec<_>, Vec<_>) = futures::join!(fut1, fut2, fut3);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn tee(self, buf_size: usize) -> Tee<Self::Item>
    where
        Self::Item: Clone;

    /// Converts to a [guard](BroadcastGuard) that can create receivers,
    /// each receiving cloned elements from this stream.
    ///
    /// The generated receivers can produce elements only after the guard is dropped.
    /// It ensures the receivers start receiving elements at the mean time.
    ///
    /// Each receiver maintains an internal buffer with `buf_size`. If one of the receiver buffer
    /// is full, the stream will halt until the blocking buffer spare the space.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     let mut guard = stream::iter(0..).broadcast(2);
    ///     let rx1 = guard.register();
    ///     let rx2 = guard.register();
    ///     guard.finish(); // drop the guard
    ///
    ///     let (ret1, ret2): (Vec<_>, Vec<_>) =
    ///         futures::join!(rx1.take(100).collect(), rx2.take(100).collect());
    ///     let expect: Vec<_> = (0..100).collect();
    ///
    ///     assert_eq!(ret1, expect);
    ///     assert_eq!(ret2, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn broadcast(self, buf_size: usize) -> BroadcastGuard<Self::Item>
    where
        Self::Item: Clone;

    /// Computes new items from the stream asynchronously in parallel with respect to the input order.
    ///
    /// The `limit` is the number of parallel workers.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    /// The method guarantees the order of output items obeys that of input items.
    ///
    /// Each parallel task runs in two-stage manner. The `f` closure is invoked in the
    /// main thread and lets you clone over outer varaibles. Then, `f` returns a future
    /// and the future will be sent to a parallel worker.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     let doubled: Vec<_> = stream::iter(0..1000)
    ///         // doubles the values in parallel
    ///         .par_then(None, move |value| async move { value * 2 })
    ///         // the collected values will be ordered
    ///         .collect()
    ///         .await;
    ///     let expect: Vec<_> = (0..1000).map(|value| value * 2).collect();
    ///     assert_eq!(doubled, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_then<P, T, F, Fut>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send + Clone,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams;

    /// Creates a parallel stream with in-local thread initializer.
    fn par_then_init<P, T, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        map_f: MapF,
    ) -> BoxStream<'static, T>
    where
        P: IntoParStreamParams,
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send;

    /// Computes new items from the stream asynchronously in parallel without respecting the input order.
    ///
    /// The `limit` is the number of parallel workers.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    /// The order of output items is not guaranteed to respect the order of input items.
    ///
    /// Each parallel task runs in two-stage manner. The `f` closure is invoked in the
    /// main thread and lets you clone over outer varaibles. Then, `f` returns a future
    /// and the future will be sent to a parallel worker.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    /// use std::collections::HashSet;
    ///
    /// async fn main_async() {
    ///     let doubled: HashSet<_> = stream::iter(0..1000)
    ///         // doubles the values in parallel
    ///         .par_then_unordered(None, move |value| {
    ///             // the future is sent to a parallel worker
    ///             async move { value * 2 }
    ///         })
    ///         // the collected values may NOT be ordered
    ///         .collect()
    ///         .await;
    ///     let expect: HashSet<_> = (0..1000).map(|value| value * 2).collect();
    ///     assert_eq!(doubled, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_then_unordered<P, T, F, Fut>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams;

    /// Creates a stream analogous to [par_then_unordered](ParStreamExt::par_then_unordered) with
    /// in-local thread initializer.
    fn par_then_init_unordered<P, T, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        map_f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams;

    /// Computes new items in a function in parallel with respect to the input order.
    ///
    /// The `limit` is the number of parallel workers.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    /// The method guarantees the order of output items obeys that of input items.
    ///
    /// Each parallel task runs in two-stage manner. The `f` closure is invoked in the
    /// main thread and lets you clone over outer varaibles. Then, `f` returns a closure
    /// and the closure will be sent to a parallel worker.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     // the variable will be shared by parallel workers
    ///     let doubled: Vec<_> = stream::iter(0..1000)
    ///         // doubles the values in parallel
    ///         .par_map(None, move |value| {
    ///             // the closure is sent to parallel worker
    ///             move || value * 2
    ///         })
    ///         // the collected values may NOT be ordered
    ///         .collect()
    ///         .await;
    ///     let expect: Vec<_> = (0..1000).map(|value| value * 2).collect();
    ///     assert_eq!(doubled, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_map<P, T, F, Func>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams;

    /// Creates a parallel stream analogous to [par_map](ParStreamExt::par_map) with
    /// in-local thread initializer.
    fn par_map_init<P, T, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams;

    /// Computes new items in a function in parallel without respecting the input order.
    ///
    /// The `limit` is the number of parallel workers.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    /// The method guarantees the order of output items obeys that of input items.
    ///
    /// Each parallel task runs in two-stage manner. The `f` closure is invoked in the
    /// main thread and lets you clone over outer varaibles. Then, `f` returns a future
    /// and the future will be sent to a parallel worker.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    /// use std::collections::HashSet;
    ///
    /// async fn main_async() {
    ///     // the variable will be shared by parallel workers
    ///
    ///     let doubled: HashSet<_> = stream::iter(0..1000)
    ///         // doubles the values in parallel
    ///         .par_map_unordered(None, move |value| {
    ///             // the closure is sent to parallel worker
    ///             move || value * 2
    ///         })
    ///         // the collected values may NOT be ordered
    ///         .collect()
    ///         .await;
    ///     let expect: HashSet<_> = (0..1000).map(|value| value * 2).collect();
    ///     assert_eq!(doubled, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_map_unordered<P, T, F, Func>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams;

    /// Creates a parallel stream analogous to [par_map_unordered](ParStreamExt::par_map_unordered) with
    /// in-local thread initializer.
    fn par_map_init_unordered<P, T, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams;

    /// Reduces the input items into single value in parallel.
    ///
    /// The `limit` is the number of parallel workers.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    ///
    /// The `buf_size` is the size of buffer that stores the temporary reduced values.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    ///
    /// Unlike [fold()](streamExt::fold), the method does not combine the values sequentially.
    /// Instead, the parallel workers greedly take two values from the buffer, reduce to
    /// one value, and push back to the buffer.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     // the variable will be shared by parallel workers
    ///     let sum = stream::iter(1..=1000)
    ///         // sum up the values in parallel
    ///         .par_reduce(None, move |lhs, rhs| {
    ///             // the closure is sent to parallel worker
    ///             async move { lhs + rhs }
    ///         })
    ///         .await;
    ///     assert_eq!(sum, Some((1 + 1000) * 1000 / 2));
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_reduce<P, F, Fut>(self, config: P, reduce_fn: F) -> ParReduce<Self::Item>
    where
        P: IntoParStreamParams,
        F: 'static + FnMut(Self::Item, Self::Item) -> Fut + Send + Clone,
        Fut: 'static + Future<Output = Self::Item> + Send;

    /// Distributes input items to specific workers and compute new items with respect to the input order.
    ///
    ///
    /// The `buf_size` is the size of input buffer before each mapping function.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    ///
    /// `routing_fn` assigns input items to specific indexes of mapping functions.
    /// `routing_fn` is executed on the calling thread.
    ///
    /// `map_fns` is a vector of mapping functions, each of which produces an asynchronous closure.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    /// use std::{future::Future, pin::Pin};
    ///
    /// async fn main_async() {
    ///     let map_fns: Vec<
    ///         Box<dyn FnMut(usize) -> Pin<Box<dyn Future<Output = usize> + Send>> + Send>,
    ///     > = vec![
    ///         // even number processor
    ///         Box::new(|even_value| Box::pin(async move { even_value / 2 })),
    ///         // odd number processor
    ///         Box::new(|odd_value| Box::pin(async move { odd_value * 2 + 1 })),
    ///     ];
    ///
    ///     let transformed: Vec<_> = stream::iter(0..1000)
    ///         // doubles the values in parallel
    ///         .par_routing(
    ///             None,
    ///             move |value| {
    ///                 // distribute the value according to its parity
    ///                 if value % 2 == 0 {
    ///                     0
    ///                 } else {
    ///                     1
    ///                 }
    ///             },
    ///             map_fns,
    ///         )
    ///         // the collected values may NOT be ordered
    ///         .collect()
    ///         .await;
    ///     let expect: Vec<_> = (0..1000)
    ///         .map(|value| {
    ///             if value % 2 == 0 {
    ///                 value / 2
    ///             } else {
    ///                 value * 2 + 1
    ///             }
    ///         })
    ///         .collect();
    ///     assert_eq!(transformed, expect);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn par_routing<F1, F2, Fut, T>(
        self,
        buf_size: impl Into<Option<usize>>,
        routing_fn: F1,
        map_fns: Vec<F2>,
    ) -> BoxStream<'static, T>
    where
        F1: 'static + FnMut(&Self::Item) -> usize + Send,
        F2: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        T: 'static + Send;

    /// Distributes input items to specific workers and compute new items without respecting the input order.
    ///
    ///
    /// The `buf_size` is the size of input buffer before each mapping function.
    /// If it is `0` or `None`, it defaults the number of cores on system.
    ///
    /// `routing_fn` assigns input items to specific indexes of mapping functions.
    /// `routing_fn` is executed on the calling thread.
    ///
    /// `map_fns` is a vector of mapping functions, each of which produces an asynchronous closure.
    fn par_routing_unordered<F1, F2, Fut, T>(
        self,
        buf_size: impl Into<Option<usize>>,
        routing_fn: F1,
        map_fns: Vec<F2>,
    ) -> BoxStream<'static, T>
    where
        F1: 'static + FnMut(&Self::Item) -> usize + Send,
        F2: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        T: 'static + Send;

    /// Splits the stream into a receiver and a future.
    ///
    /// The returned future scatters input items into the receiver and its clones,
    /// and should be manually awaited by user.
    ///
    /// The returned receiver can be cloned and distributed to resepctive workers.
    ///
    /// It lets user to write custom workers that receive items from the same stream.
    ///
    /// ```rust
    /// use futures::prelude::*;
    /// use par_stream::prelude::*;
    ///
    /// async fn main_async() {
    ///     let orig = stream::iter(1isize..=1000);
    ///
    ///     // scatter the items
    ///     let rx1 = orig.scatter();
    ///     let rx2 = rx1.clone();
    ///
    ///     // collect the values concurrently
    ///     let (values1, values2): (Vec<_>, Vec<_>) = futures::join!(rx1.collect(), rx2.collect());
    ///
    ///     // the total item count is equal to the original set
    ///     assert_eq!(values1.len() + values2.len(), 1000);
    /// }
    ///
    /// # #[cfg(feature = "runtime-async-std")]
    /// # #[async_std::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-tokio")]
    /// # #[tokio::main]
    /// # async fn main() {
    /// #     main_async().await
    /// # }
    /// #
    /// # #[cfg(feature = "runtime-smol")]
    /// # fn main() {
    /// #     smol::block_on(main_async())
    /// # }
    /// ```
    fn scatter(self) -> Scatter<Self::Item>
where;

    /// Runs an asynchronous task on each element of an stream in parallel.
    fn par_for_each<P, F, Fut>(self, config: P, f: F) -> ParForEach
    where
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = ()> + Send,
        P: IntoParStreamParams;

    /// Creates a parallel stream analogous to [par_for_each](ParStreamExt::par_for_each) with a
    /// in-local thread initializer.
    fn par_for_each_init<P, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        map_f: MapF,
    ) -> ParForEach
    where
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = ()> + Send,
        P: IntoParStreamParams;

    /// Runs an blocking task on each element of an stream in parallel.
    fn par_for_each_blocking<P, F, Func>(self, config: P, f: F) -> ParForEachBlocking
    where
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() + Send,
        P: IntoParStreamParams;

    /// Creates a parallel stream analogous to [par_for_each_blocking](ParStreamExt::par_for_each_blocking) with a
    /// in-local thread initializer.
    fn par_for_each_blocking_init<P, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        f: MapF,
    ) -> ParForEachBlocking
    where
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() + Send,
        P: IntoParStreamParams;
}

impl<S> ParStreamExt for S
where
    S: 'static + Send + Stream + StreamExt,
    S::Item: 'static + Send,
{
    fn scan_spawned<B, T, F, Fut>(
        self,
        buf_size: impl Into<Option<usize>>,
        init: B,
        mut map_fn: F,
    ) -> BoxStream<'static, T>
    where
        B: 'static + Send,
        T: 'static + Send,
        F: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: Future<Output = Option<(B, T)>> + Send,
    {
        let buf_size = buf_size.into().unwrap_or(2);
        let (tx, rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let mut state = init;
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                match map_fn(state, item).await {
                    Some((new_state, output)) => {
                        state = new_state;
                        if tx.send_async(output).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });

        rx.into_stream().boxed()
    }

    fn then_spawned<T, F, Fut>(
        self,
        buf_size: impl Into<Option<usize>>,
        f: F,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: Future<Output = T> + Send,
    {
        let buf_size = buf_size.into().unwrap_or(2);
        let (tx, rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let _ = self.then(f).map(Ok).forward(tx.into_sink()).await;
        });

        rx.into_stream().boxed()
    }

    fn map_spawned<T, F>(self, buf_size: impl Into<Option<usize>>, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> T + Send,
    {
        let buf_size = buf_size.into().unwrap_or(2);
        let (tx, rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let _ = self.map(f).map(Ok).forward(tx.into_sink()).await;
        });

        rx.into_stream().boxed()
    }

    fn batching<T, F, Fut>(self, f: F) -> BoxStream<'static, T>
    where
        F: FnOnce(flume::Receiver<Self::Item>, flume::Sender<T>) -> Fut,
        Fut: 'static + Future<Output = ()> + Send,
        T: 'static + Send,
    {
        let (input_tx, input_rx) = flume::bounded(0);
        let (output_tx, output_rx) = flume::bounded(0);

        let input_future = async move {
            let _ = self.map(Ok).forward(input_tx.into_sink()).await;
        };
        let batching_future = f(input_rx, output_tx);
        let join_future = future::join(input_future, batching_future);

        utils::join_future_stream(join_future, output_rx.into_stream()).boxed()
    }

    fn par_batching_unordered<P, T, F, Fut>(self, config: P, mut f: F) -> BoxStream<'static, T>
    where
        F: FnMut(usize, flume::Receiver<Self::Item>, flume::Sender<T>) -> Fut,
        Fut: 'static + Future<Output = ()> + Send,
        T: 'static + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();

        let (input_tx, input_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let _ = self.map(Ok).forward(input_tx.into_sink()).await;
        });

        (0..num_workers).for_each(|worker_index| {
            let fut = f(worker_index, input_rx.clone(), output_tx.clone());
            rt::spawn(fut);
        });

        output_rx.into_stream().boxed()
    }

    fn tee(self, buf_size: usize) -> Tee<Self::Item>
    where
        Self::Item: Clone,
    {
        let buf_size = buf_size.into();
        let (tx, rx) = mpsc::channel(buf_size);
        let sender_set = Arc::new(flurry::HashSet::new());
        let guard = sender_set.guard();
        sender_set.insert(ByAddress(Arc::new(tx)), &guard);

        let future = {
            let sender_set = sender_set.clone();
            let mut stream = self.boxed();

            let future = rt::spawn(async move {
                while let Some(item) = stream.next().await {
                    let futures: Vec<_> = sender_set
                        .pin()
                        .iter()
                        .map(|tx| {
                            let tx = tx.clone();
                            let item = item.clone();
                            async move {
                                let result = tx.send(item).await;
                                (result, tx)
                            }
                        })
                        .collect();

                    let results = future::join_all(futures).await;
                    let success_count = results
                        .iter()
                        .filter(|(result, tx)| {
                            let ok = result.is_ok();
                            if !ok {
                                sender_set.pin().remove(tx);
                            }
                            ok
                        })
                        .count();

                    if success_count == 0 {
                        break;
                    }
                }
            });

            Arc::new(Mutex::new(Some(future)))
        };

        Tee {
            future,
            sender_set: Arc::downgrade(&sender_set),
            stream: rx.into_stream(),
            buf_size,
        }
    }

    fn broadcast(self, buf_size: usize) -> BroadcastGuard<Self::Item>
    where
        Self::Item: Clone,
    {
        let (init_tx, init_rx) = oneshot::channel();
        let ready = Arc::new(AtomicBool::new(false));

        let future = rt::spawn(async move {
            let mut senders: Vec<mpsc::Sender<_>> = match init_rx.await {
                Ok(senders) => senders,
                Err(_) => return,
            };

            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let sending_futures = senders.into_iter().map(move |tx| {
                    let item = item.clone();
                    async move {
                        tx.send(item).await.ok()?;
                        Some(tx)
                    }
                });
                let senders_: Option<Vec<_>> = future::join_all(sending_futures)
                    .await
                    .into_iter()
                    .collect();

                match senders_ {
                    Some(senders_) => {
                        senders = senders_;
                    }
                    None => break,
                }
            }
        })
        .map(|result| result.unwrap())
        .boxed();

        BroadcastGuard {
            buf_size,
            ready,
            init_tx: Some(init_tx),
            future: Arc::new(Mutex::new(Some(future))),
            senders: Some(vec![]),
        }
    }

    fn par_then<P, T, F, Fut>(self, config: P, mut f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams,
    {
        let indexed_f = move |(index, item)| {
            let fut = f(item);
            fut.map(move |output| (index, output))
        };

        self.enumerate()
            .par_then_unordered(config, indexed_f)
            .reorder_enumerated()
            .boxed()
    }

    fn par_then_init<P, T, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        mut map_f: MapF,
    ) -> BoxStream<'static, T>
    where
        P: IntoParStreamParams,
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
    {
        self.enumerate()
            .par_then_init_unordered(config, init_f, move |init, (index, item)| {
                let fut = map_f(init, item);
                async move { (index, fut.await) }
            })
            .reorder_enumerated()
            .boxed()
    }

    fn par_then_unordered<P, T, F, Fut>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (input_tx, input_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let _ = self.map(f).map(Ok).forward(input_tx.into_sink()).await;
        });
        (0..num_workers).for_each(|_| {
            let input_rx = input_rx.clone();
            let output_tx = output_tx.clone();

            rt::spawn(async move {
                let _ = input_rx
                    .into_stream()
                    .then(|fut| fut)
                    .map(Ok)
                    .forward(output_tx.into_sink())
                    .await;
            });
        });
        output_rx.into_stream().boxed()
    }

    fn par_then_init_unordered<P, T, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        mut map_f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (input_tx, input_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);
        let init = init_f();

        rt::spawn(async move {
            let _ = self
                .map(move |item| map_f(init.clone(), item))
                .map(Ok)
                .forward(input_tx.into_sink())
                .await;
        });

        (0..num_workers).for_each(|_| {
            let input_rx = input_rx.clone();
            let output_tx = output_tx.clone();

            rt::spawn(async move {
                let _ = input_rx
                    .into_stream()
                    .then(|fut| fut)
                    .map(Ok)
                    .forward(output_tx.into_sink())
                    .await;
            });
        });

        output_rx.into_stream().boxed()
    }

    fn par_map<P, T, F, Func>(self, config: P, mut f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams,
    {
        self.enumerate()
            .par_map_unordered(config, move |(index, item)| {
                let job = f(item);
                move || (index, job())
            })
            .reorder_enumerated()
            .boxed()
    }

    fn par_map_init<P, T, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        mut map_f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams,
    {
        self.enumerate()
            .par_map_init_unordered(config, init_f, move |init, (index, item)| {
                let job = map_f(init, item);
                move || (index, job())
            })
            .reorder_enumerated()
            .boxed()
    }

    fn par_map_unordered<P, T, F, Func>(self, config: P, f: F) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (input_tx, input_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);

        rt::spawn(async move {
            let _ = self.map(f).map(Ok).forward(input_tx.into_sink()).await;
        });

        (0..num_workers).for_each(|_| {
            let input_rx = input_rx.clone();
            let output_tx = output_tx.clone();

            rt::spawn_blocking(move || {
                while let Ok(job) = input_rx.recv() {
                    let output = job();
                    let result = output_tx.send(output);
                    if result.is_err() {
                        break;
                    }
                }
            });
        });

        output_rx.into_stream().boxed()
    }

    fn par_map_init_unordered<P, T, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        mut f: MapF,
    ) -> BoxStream<'static, T>
    where
        T: 'static + Send,
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() -> T + Send,
        P: IntoParStreamParams,
    {
        let init = init_f();

        self.enumerate()
            .par_map_unordered(config, move |(index, item)| {
                let job = f(init.clone(), item);
                move || (index, job())
            })
            .reorder_enumerated()
            .boxed()
    }

    fn par_reduce<P, F, Fut>(self, config: P, reduce_fn: F) -> ParReduce<Self::Item>
    where
        P: IntoParStreamParams,
        F: 'static + FnMut(Self::Item, Self::Item) -> Fut + Send + Clone,
        Fut: 'static + Future<Output = Self::Item> + Send,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();

        // phase 1
        let phase_1_future = async move {
            let (input_tx, input_rx) = flume::bounded(buf_size);

            let input_future = rt::spawn(async move {
                let _ = self.map(Ok).forward(input_tx.into_sink()).await;
            })
            .map(|result| result.unwrap());

            let reducer_futures = {
                let reduce_fn = reduce_fn.clone();

                (0..num_workers).map(move |_| {
                    let input_rx = input_rx.clone();
                    let mut reduce_fn = reduce_fn.clone();

                    rt::spawn(async move {
                        let mut reduced = input_rx.recv_async().await.ok()?;

                        while let Ok(item) = input_rx.recv_async().await {
                            reduced = reduce_fn(reduced, item).await;
                        }

                        Some(reduced)
                    })
                    .map(|result| result.unwrap())
                })
            };
            let join_reducer_future = future::join_all(reducer_futures);

            let ((), values) = future::join(input_future, join_reducer_future).await;

            (values, reduce_fn)
        };

        // phase 2
        let phase_2_future = async move {
            let (values, reduce_fn) = phase_1_future.await;

            let (pair_tx, pair_rx) = flume::bounded(buf_size);
            let (feedback_tx, feedback_rx) = flume::bounded(num_workers);

            let mut count = 0;

            for value in values {
                if let Some(value) = value {
                    feedback_tx.send_async(value).await.map_err(|_| ()).unwrap();
                    count += 1;
                }
            }

            let pairing_future = {
                rt::spawn(async move {
                    while count >= 2 {
                        let first = feedback_rx.recv_async().await.unwrap();
                        let second = feedback_rx.recv_async().await.unwrap();
                        pair_tx.send_async((first, second)).await.unwrap();
                        count -= 1;
                    }

                    match count {
                        0 => None,
                        1 => {
                            let output = feedback_rx.recv_async().await.unwrap();
                            Some(output)
                        }
                        _ => unreachable!(),
                    }
                })
                .map(|result| result.unwrap())
            };

            let reducer_futures = (0..num_workers).map(move |_| {
                let pair_rx = pair_rx.clone();
                let feedback_tx = feedback_tx.clone();
                let mut reduce_fn = reduce_fn.clone();

                rt::spawn(async move {
                    while let Ok((first, second)) = pair_rx.recv_async().await {
                        let reduced = reduce_fn(first, second).await;
                        feedback_tx
                            .send_async(reduced)
                            .await
                            .map_err(|_| ())
                            .unwrap();
                    }
                })
                .map(|result| result.unwrap())
            });
            let join_reducer_future = future::join_all(reducer_futures);

            let (output, _) = future::join(pairing_future, join_reducer_future).await;

            output
        };

        let future = phase_2_future.boxed();

        ParReduce { future }
    }

    fn par_routing<F1, F2, Fut, T>(
        self,
        buf_size: impl Into<Option<usize>>,
        mut routing_fn: F1,
        mut map_fns: Vec<F2>,
    ) -> BoxStream<'static, T>
    where
        F1: 'static + FnMut(&Self::Item) -> usize + Send,
        F2: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        T: 'static + Send,
    {
        let buf_size = match buf_size.into() {
            None | Some(0) => num_cpus::get(),
            Some(size) => size,
        };

        let (reorder_tx, reorder_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);

        let (mut map_txs, map_futs): (Vec<_>, Vec<_>) = map_fns
            .iter()
            .map(|_| {
                let (map_tx, map_rx) = flume::bounded(buf_size);
                let reorder_tx = reorder_tx.clone();

                let map_fut = rt::spawn(async move {
                    while let Ok((counter, fut)) = map_rx.recv_async().await {
                        let output = fut.await;
                        if reorder_tx.send_async((counter, output)).await.is_err() {
                            break;
                        };
                    }
                })
                .map(|result| result.unwrap());

                (map_tx, map_fut)
            })
            .unzip();

        let routing_fut = async move {
            let mut counter = 0u64;
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let index = routing_fn(&item);
                let map_fn = map_fns
                    .get_mut(index)
                    .expect("the routing function returns an invalid index");
                let map_tx = map_txs.get_mut(index).unwrap();
                let fut = map_fn(item);
                if map_tx.send_async((counter, fut)).await.is_err() {
                    break;
                };

                counter = counter.wrapping_add(1);
            }
        };

        let reorder_fut = async move {
            let mut counter = 0u64;
            let mut pool = HashMap::new();

            while let Ok((index, output)) = reorder_rx.recv_async().await {
                if index != counter {
                    pool.insert(index, output);
                    continue;
                }

                if output_tx.send_async(output).await.is_err() {
                    break;
                };
                counter = counter.wrapping_add(1);

                while let Some(output) = pool.remove(&counter) {
                    if output_tx.send_async(output).await.is_err() {
                        break;
                    };
                    counter = counter.wrapping_add(1);
                }
            }
        };

        let join_fut = future::join3(routing_fut, reorder_fut, future::join_all(map_futs)).boxed();

        utils::join_future_stream(join_fut, output_rx.into_stream()).boxed()
    }

    fn par_routing_unordered<F1, F2, Fut, T>(
        self,
        buf_size: impl Into<Option<usize>>,
        mut routing_fn: F1,
        mut map_fns: Vec<F2>,
    ) -> BoxStream<'static, T>
    where
        F1: 'static + FnMut(&Self::Item) -> usize + Send,
        F2: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = T> + Send,
        T: 'static + Send,
    {
        let buf_size = match buf_size.into() {
            None | Some(0) => num_cpus::get(),
            Some(size) => size,
        };

        let (output_tx, output_rx) = flume::bounded(buf_size);

        let (mut map_txs, map_futs): (Vec<_>, Vec<_>) = map_fns
            .iter()
            .map(|_| {
                let (map_tx, map_rx) = flume::bounded(buf_size);
                let output_tx = output_tx.clone();

                let map_fut = rt::spawn(async move {
                    while let Ok(fut) = map_rx.recv_async().await {
                        let output = fut.await;
                        if output_tx.send_async(output).await.is_err() {
                            break;
                        };
                    }
                })
                .map(|result| result.unwrap());

                (map_tx, map_fut)
            })
            .unzip();

        let routing_fut = async move {
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let index = routing_fn(&item);
                let map_fn = map_fns
                    .get_mut(index)
                    .expect("the routing function returns an invalid index");
                let map_tx = map_txs.get_mut(index).unwrap();
                let fut = map_fn(item);
                if map_tx.send_async(fut).await.is_err() {
                    break;
                };
            }
        };

        let join_fut = future::join(routing_fut, future::join_all(map_futs));

        utils::join_future_stream(join_fut, output_rx.into_stream()).boxed()
    }

    fn scatter(self) -> Scatter<Self::Item> {
        let (tx, rx) = flume::bounded(0);

        rt::spawn(async move {
            let _ = self.map(Ok).forward(tx.into_sink()).await;
        });

        Scatter {
            stream: rx.into_stream(),
        }
    }

    fn par_for_each<P, F, Fut>(self, config: P, mut f: F) -> ParForEach
    where
        F: 'static + FnMut(Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = ()> + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (map_tx, map_rx) = flume::bounded(buf_size);

        let map_fut = async move {
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let fut = f(item);
                if map_tx.send_async(fut).await.is_err() {
                    break;
                }
            }
        };

        let worker_futs: Vec<_> = (0..num_workers)
            .map(|_| {
                let map_rx = map_rx.clone();

                let worker_fut = async move {
                    while let Ok(fut) = map_rx.recv_async().await {
                        fut.await;
                    }
                };
                rt::spawn(worker_fut).map(|result| result.unwrap())
            })
            .collect();

        let join_fut = future::join(map_fut, future::join_all(worker_futs))
            .map(|_| ())
            .boxed();

        ParForEach { future: join_fut }
    }

    fn par_for_each_init<P, B, InitF, MapF, Fut>(
        self,
        config: P,
        init_f: InitF,
        mut map_f: MapF,
    ) -> ParForEach
    where
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Fut + Send,
        Fut: 'static + Future<Output = ()> + Send,
        P: IntoParStreamParams,
    {
        let init = init_f();
        self.par_for_each(config, move |item| map_f(init.clone(), item))
    }

    fn par_for_each_blocking<P, F, Func>(self, config: P, mut f: F) -> ParForEachBlocking
    where
        F: 'static + FnMut(Self::Item) -> Func + Send,
        Func: 'static + FnOnce() + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (map_tx, map_rx) = flume::bounded(buf_size);

        let map_fut = async move {
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let fut = f(item);
                if map_tx.send_async(fut).await.is_err() {
                    break;
                }
            }
        };

        let worker_futs: Vec<_> = (0..num_workers)
            .map(|_| {
                let map_rx = map_rx.clone();
                rt::spawn_blocking(move || {
                    while let Ok(job) = map_rx.recv() {
                        job();
                    }
                })
                .map(|result| result.unwrap())
            })
            .collect();

        let join_fut = future::join(map_fut, future::join_all(worker_futs))
            .map(|_| ())
            .boxed();

        ParForEachBlocking { future: join_fut }
    }

    fn par_for_each_blocking_init<P, B, InitF, MapF, Func>(
        self,
        config: P,
        init_f: InitF,
        mut map_f: MapF,
    ) -> ParForEachBlocking
    where
        B: 'static + Send + Clone,
        InitF: FnOnce() -> B,
        MapF: 'static + FnMut(B, Self::Item) -> Func + Send,
        Func: 'static + FnOnce() + Send,
        P: IntoParStreamParams,
    {
        let init = init_f();
        self.par_for_each_blocking(config, move |item| map_f(init.clone(), item))
    }
}

// iter_spawned

pub use iter_spawned::*;

mod iter_spawned {
    use super::*;

    /// Converts an [Iterator] into a [Stream] by consuming the iterator in a blocking thread.
    ///
    /// It is useful when consuming the iterator is computationally expensive and involves blocking code.
    /// It prevents blocking the asynchronous context when consuming the returned stream.
    pub fn iter_spawned<I>(buf_size: usize, iter: I) -> IterSpawned<I::Item>
    where
        I: 'static + IntoIterator + Send,
        I::Item: Send,
    {
        let (tx, rx) = flume::bounded(buf_size);

        rt::spawn_blocking(move || {
            for item in iter.into_iter() {
                if tx.send(item).is_err() {
                    break;
                }
            }
        });

        IterSpawned {
            stream: rx.into_stream(),
        }
    }

    /// Stream for the [iter_spawned] function.
    #[derive(Clone)]
    pub struct IterSpawned<T>
    where
        T: 'static,
    {
        stream: flume::r#async::RecvStream<'static, T>,
    }

    impl<T> Stream for IterSpawned<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// sync

pub use sync::*;

mod sync {
    use super::*;
    use std::{cmp::Reverse, collections::BinaryHeap};

    #[derive(Derivative)]
    #[derivative(PartialEq, Eq, PartialOrd, Ord)]
    struct KV<K, V> {
        pub key: K,
        pub index: usize,
        #[derivative(PartialEq = "ignore", PartialOrd = "ignore", Ord = "ignore")]
        pub value: V,
    }

    /// Synchronize streams by pairing up keys of each stream item.
    ///
    /// The `key_fn` constructs the key for each item.
    /// The input items are grouped by their keys in the interal buffer until
    /// all items with the key arrives. The finished items are yielded in type
    /// `Ok((stream_index, item))` in monotonic manner.
    ///
    /// If any one of the `streams` generates a non-monotonic item. The item is
    /// yielded as `Err((stream_index, item))` immediately.
    pub fn sync_by_key<I, F, K, S>(
        buf_size: impl Into<Option<usize>>,
        key_fn: F,
        streams: I,
    ) -> Sync<S::Item>
    where
        I: IntoIterator<Item = S>,
        S: 'static + Stream + Send,
        S::Item: 'static + Send,
        F: 'static + Fn(&S::Item) -> K + Send,
        K: 'static + Clone + Ord + Send,
    {
        let buf_size = buf_size.into().unwrap_or_else(|| num_cpus::get());

        let streams: Vec<_> = streams
            .into_iter()
            .enumerate()
            .map(|(index, stream)| stream.map(move |item| (index, item)).boxed())
            .collect();
        let num_streams = streams.len();

        match num_streams {
            0 => {
                return Sync {
                    stream: stream::empty().boxed(),
                };
            }
            1 => {
                return Sync {
                    stream: streams.into_iter().next().unwrap().map(Ok).boxed(),
                };
            }
            _ => {}
        }

        let mut select_stream = stream::select_all(streams);
        let (input_tx, input_rx) = flume::bounded(buf_size);
        let (output_tx, output_rx) = flume::bounded(buf_size);

        let input_future = async move {
            while let Some((index, item)) = select_stream.next().await {
                let key = key_fn(&item);
                if input_tx.send_async((index, key, item)).await.is_err() {
                    break;
                }
            }
        };

        let sync_future = rt::spawn_blocking(move || {
            let mut heap: BinaryHeap<Reverse<KV<K, S::Item>>> = BinaryHeap::new();
            let mut min_items: Vec<Option<K>> = vec![None; num_streams];
            let mut threshold: Option<K>;

            'worker: loop {
                'input: while let Ok((index, key, item)) = input_rx.recv() {
                    // update min item for that stream
                    {
                        let prev = &mut min_items[index];
                        match prev {
                            Some(prev) if *prev <= key => {
                                *prev = key.clone();
                            }
                            Some(_) => {
                                let ok = output_tx.send(Err((index, item))).is_ok();
                                if !ok {
                                    break 'worker;
                                }
                                continue 'input;
                            }
                            None => *prev = Some(key.clone()),
                        }
                    }

                    // save item
                    heap.push(Reverse(KV {
                        index,
                        key,
                        value: item,
                    }));

                    // update global threshold
                    threshold = min_items.iter().min().unwrap().clone();

                    // pop items below threshold
                    if let Some(threshold) = &threshold {
                        'output: while let Some(Reverse(KV { key, .. })) = heap.peek() {
                            if key < threshold {
                                let KV { value, index, .. } = heap.pop().unwrap().0;
                                let ok = output_tx.send(Ok((index, value))).is_ok();
                                if !ok {
                                    break 'worker;
                                }
                            } else {
                                break 'output;
                            }
                        }
                    }
                }

                // send remaining items
                for Reverse(KV { index, value, .. }) in heap {
                    let ok = output_tx.send(Ok((index, value))).is_ok();
                    if !ok {
                        break 'worker;
                    }
                }

                break;
            }
        })
        .map(|result| result.unwrap());

        let join_future = future::join(input_future, sync_future);

        let stream = utils::join_future_stream(join_future, output_rx.into_stream()).boxed();

        Sync { stream }
    }

    /// A stream combinator returned from [sync_by_key()](super::sync_by_key()).
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct Sync<T> {
        #[derivative(Debug = "ignore")]
        pub(super) stream: BoxStream<'static, Result<(usize, T), (usize, T)>>,
    }

    impl<T> Stream for Sync<T> {
        type Item = Result<(usize, T), (usize, T)>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// unfold

pub use unfold::*;

mod unfold {
    use super::*;

    /// Creates a stream with elements produced by an asynchronous function.
    ///
    /// The `init_f` function creates an initial state. Then `unfold_f` consumes the state
    /// and is called repeatedly. If `unfold_f` returns `Some(output, state)`, it produces
    /// the output as stream element and updates the state, until it returns `None`.
    pub fn unfold<IF, UF, IFut, UFut, State, Item>(
        buf_size: impl Into<Option<usize>>,
        mut init_f: IF,
        mut unfold_f: UF,
    ) -> Unfold<Item>
    where
        IF: 'static + FnMut() -> IFut + Send,
        UF: 'static + FnMut(State) -> UFut + Send,
        IFut: Future<Output = State> + Send,
        UFut: Future<Output = Option<(Item, State)>> + Send,
        State: Send,
        Item: 'static + Send,
    {
        let buf_size = buf_size.into().unwrap_or_else(num_cpus::get);
        let (data_tx, data_rx) = flume::bounded(buf_size);

        let producer_fut = rt::spawn(async move {
            let mut state = init_f().await;

            while let Some((item, new_state)) = unfold_f(state).await {
                let result = data_tx.send_async(item).await;
                if result.is_err() {
                    break;
                }
                state = new_state;
            }
        });

        let stream = stream::select(
            producer_fut
                .into_stream()
                .map(|result| {
                    if let Err(err) = result {
                        panic!("unable to spawn a worker: {:?}", err);
                    }
                    None
                })
                .fuse(),
            data_rx.into_stream().map(Some),
        )
        .filter_map(|item| async move { item })
        .boxed();

        Unfold { stream }
    }

    /// Creates a stream with elements produced by a function.
    ///
    /// The `init_f` function creates an initial state. Then `unfold_f` consumes the state
    /// and is called repeatedly. If `unfold_f` returns `Some(output, state)`, it produces
    /// the output as stream element and updates the state, until it returns `None`.
    pub fn unfold_blocking<IF, UF, State, Item>(
        buf_size: impl Into<Option<usize>>,
        mut init_f: IF,
        mut unfold_f: UF,
    ) -> Unfold<Item>
    where
        IF: 'static + FnMut() -> State + Send,
        UF: 'static + FnMut(State) -> Option<(Item, State)> + Send,
        Item: 'static + Send,
    {
        let buf_size = buf_size.into().unwrap_or_else(num_cpus::get);
        let (data_tx, data_rx) = flume::bounded(buf_size);

        let producer_fut = rt::spawn_blocking(move || {
            let mut state = init_f();

            while let Some((item, new_state)) = unfold_f(state) {
                let result = data_tx.send(item);
                if result.is_err() {
                    break;
                }
                state = new_state;
            }
        });

        let stream = stream::select(
            producer_fut
                .into_stream()
                .map(|result| {
                    if let Err(err) = result {
                        panic!("unable to spawn a worker: {:?}", err);
                    }
                    None
                })
                .fuse(),
            data_rx.into_stream().map(Some),
        )
        .filter_map(|item| async move { item })
        .boxed();

        Unfold { stream }
    }

    /// A stream combinator returned from [unfold()](super::unfold())
    /// or [unfold_blocking()](super::unfold_blocking()).
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct Unfold<T> {
        #[derivative(Debug = "ignore")]
        pub(super) stream: BoxStream<'static, T>,
    }

    impl<T> Stream for Unfold<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// par_unfold_unordered

pub use par_unfold_unordered::*;

mod par_unfold_unordered {
    use super::*;

    /// Creates a stream elements produced by multiple concurrent workers.
    ///
    /// Each worker obtains the initial state by calling `init_f(worker_index)`.
    /// Then, `unfold_f(wokrer_index, state)` consumes the state and is called repeatedly.
    /// If `unfold_f` returns `Some((output, state))`, the output is produced as stream element and
    /// the state is updated. The stream finishes `unfold_f` returns `None` on all workers.
    ///
    /// The output elements collected from workers can be arbitrary ordered. There is no
    /// ordering guarantee respecting to the order of function callings and worker indexes.
    pub fn par_unfold_unordered<P, IF, UF, IFut, UFut, State, Item>(
        config: P,
        mut init_f: IF,
        unfold_f: UF,
    ) -> ParUnfoldUnordered<Item>
    where
        IF: 'static + FnMut(usize) -> IFut,
        UF: 'static + FnMut(usize, State) -> UFut + Send + Clone,
        IFut: 'static + Future<Output = State> + Send,
        UFut: 'static + Future<Output = Option<(Item, State)>> + Send,
        State: Send,
        Item: 'static + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (output_tx, output_rx) = flume::bounded(buf_size);

        let worker_futs = (0..num_workers).map(|worker_index| {
            let init_fut = init_f(worker_index);
            let mut unfold_f = unfold_f.clone();
            let output_tx = output_tx.clone();

            rt::spawn(async move {
                let mut state = init_fut.await;

                loop {
                    match unfold_f(worker_index, state).await {
                        Some((item, new_state)) => {
                            let result = output_tx.send_async(item).await;
                            if result.is_err() {
                                break;
                            }
                            state = new_state;
                        }
                        None => {
                            break;
                        }
                    }
                }
            })
        });

        let join_future = future::try_join_all(worker_futs);

        let stream = stream::select(
            output_rx.into_stream().map(Some),
            join_future.into_stream().map(|result| {
                result.unwrap();
                None
            }),
        )
        .filter_map(|item| async move { item })
        .boxed();

        ParUnfoldUnordered { stream }
    }

    /// Creates a stream elements produced by multiple concurrent workers. It is a blocking analogous to
    /// [par_unfold_unordered()].
    pub fn par_unfold_blocking_unordered<P, IF, UF, State, Item>(
        config: P,
        init_f: IF,
        unfold_f: UF,
    ) -> ParUnfoldUnordered<Item>
    where
        IF: 'static + FnMut(usize) -> State + Send + Clone,
        UF: 'static + FnMut(usize, State) -> Option<(Item, State)> + Send + Clone,
        Item: 'static + Send,
        P: IntoParStreamParams,
    {
        let ParStreamParams {
            num_workers,
            buf_size,
        } = config.into_par_stream_params();
        let (output_tx, output_rx) = flume::bounded(buf_size);

        let worker_futs = (0..num_workers).map(|worker_index| {
            let mut init_f = init_f.clone();
            let mut unfold_f = unfold_f.clone();
            let output_tx = output_tx.clone();

            rt::spawn_blocking(move || {
                let mut state = init_f(worker_index);

                loop {
                    match unfold_f(worker_index, state) {
                        Some((item, new_state)) => {
                            let result = output_tx.send(item);
                            if result.is_err() {
                                break;
                            }
                            state = new_state;
                        }
                        None => {
                            break;
                        }
                    }
                }
            })
        });

        let join_future = future::try_join_all(worker_futs);

        let stream = stream::select(
            output_rx.into_stream().map(Some),
            join_future.into_stream().map(|result| {
                result.unwrap();
                None
            }),
        )
        .filter_map(|item| async move { item })
        .boxed();

        ParUnfoldUnordered { stream }
    }

    /// A stream combinator returned from [par_unfold_unordered()](super::par_unfold_unordered())
    /// and  [par_unfold_blocking_unordered()](super::par_unfold_blocking_unordered()).
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct ParUnfoldUnordered<T> {
        #[derivative(Debug = "ignore")]
        pub(super) stream: BoxStream<'static, T>,
    }

    impl<T> Stream for ParUnfoldUnordered<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// scatter

pub use scatter::*;

mod scatter {
    use super::*;

    /// A stream combinator returned from [scatter()](ParStreamExt::scatter).
    #[derive(Clone)]
    pub struct Scatter<T>
    where
        T: 'static,
    {
        pub(super) stream: flume::r#async::RecvStream<'static, T>,
    }

    impl<T> Stream for Scatter<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// tee

pub use tee::*;

mod tee {
    use super::*;
    use tokio_stream::wrappers::ReceiverStream;

    /// A stream combinator returned from [tee()](ParStreamExt::tee).
    #[derive(Debug)]
    pub struct Tee<T> {
        pub(super) buf_size: usize,
        pub(super) future: Arc<Mutex<Option<rt::JoinHandle<()>>>>,
        pub(super) sender_set: Weak<flurry::HashSet<ByAddress<Arc<mpsc::Sender<T>>>>>,
        pub(super) stream: ReceiverStream<T>,
    }

    impl<T> Clone for Tee<T>
    where
        T: 'static + Send,
    {
        fn clone(&self) -> Self {
            let buf_size = self.buf_size;
            let (tx, rx) = mpsc::channel(buf_size);
            let sender_set = self.sender_set.clone();

            if let Some(sender_set) = sender_set.upgrade() {
                let guard = sender_set.guard();
                sender_set.insert(ByAddress(Arc::new(tx)), &guard);
            }

            Self {
                future: self.future.clone(),
                sender_set,
                stream: rx.into_stream(),
                buf_size,
            }
        }
    }

    impl<T> Stream for Tee<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            if let Ok(mut future_opt) = self.future.try_lock() {
                if let Some(future) = &mut *future_opt {
                    if Pin::new(future).poll(cx).is_ready() {
                        *future_opt = None;
                    }
                }
            }

            match Pin::new(&mut self.stream).poll_next(cx) {
                Ready(Some(output)) => {
                    cx.waker().clone().wake();
                    Ready(Some(output))
                }
                Ready(None) => Ready(None),
                Pending => {
                    cx.waker().clone().wake();
                    Pending
                }
            }
        }
    }
}

// broadcast

pub use broadcast::*;

mod broadcast {
    use super::*;

    /// The guard type returned from [broadcast()](ParStreamExt::broadcast).
    ///
    /// The guard is used to register new broadcast receivers, each consuming elements
    /// from the stream. The guard must be dropped, either by `guard.finish()` or
    /// `drop(guard)` before the receivers start consuming data. Otherwise, the
    /// receivers will receive panic.
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct BroadcastGuard<T> {
        pub(super) buf_size: usize,
        pub(super) ready: Arc<AtomicBool>,
        pub(super) init_tx: Option<oneshot::Sender<Vec<mpsc::Sender<T>>>>,
        #[derivative(Debug = "ignore")]
        pub(super) future: Arc<Mutex<Option<BoxFuture<'static, ()>>>>,
        pub(super) senders: Option<Vec<mpsc::Sender<T>>>,
    }

    impl<T> BroadcastGuard<T>
    where
        T: 'static + Send,
    {
        /// Creates a new receiver.
        pub fn register(&mut self) -> BroadcastStream<T> {
            let Self {
                buf_size,
                ref future,
                ref ready,
                ref mut senders,
                ..
            } = *self;
            let senders = senders.as_mut().unwrap();

            let (tx, rx) = mpsc::channel(buf_size);
            senders.push(tx);

            let future = future.clone();
            let ready = ready.clone();

            let stream = stream::select(
                rx.into_stream().map(Some),
                async move {
                    assert!(
                        ready.load(Acquire),
                        "please call guard.finish() before consuming this stream"
                    );

                    let future = &mut *future.lock().await;
                    if let Some(future_) = future {
                        future_.await;
                        *future = None;
                    }

                    None
                }
                .into_stream(),
            )
            .filter_map(|item| async move { item })
            .boxed();

            BroadcastStream { stream }
        }

        /// Drops the guard, so that created receivers can consume data without panic.
        pub fn finish(self) {
            drop(self)
        }
    }

    impl<T> Drop for BroadcastGuard<T> {
        fn drop(&mut self) {
            let init_tx = self.init_tx.take().unwrap();
            let senders = self.senders.take().unwrap();
            let _ = init_tx.send(senders);
            self.ready.store(true, Release);
        }
    }

    /// The receiver that consumes broadcasted messages from the stream.
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct BroadcastStream<T> {
        #[derivative(Debug = "ignore")]
        pub(super) stream: BoxStream<'static, T>,
    }

    impl<T> Stream for BroadcastStream<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// reorder_enumerated

pub use reorder_enumerated::*;

mod reorder_enumerated {
    use super::*;

    /// A stream combinator returned from [reorder_enumerated()](IndexedStreamExt::reorder_enumerated).
    #[pin_project(project = ReorderEnumeratedProj)]
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct ReorderEnumerated<S, T>
    where
        S: ?Sized,
    {
        pub(super) commit: usize,
        pub(super) buffer: HashMap<usize, T>,
        #[pin]
        pub(super) stream: S,
    }

    impl<S, T> Stream for ReorderEnumerated<S, T>
    where
        S: Stream<Item = (usize, T)>,
    {
        type Item = T;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            let ReorderEnumeratedProj {
                stream,
                commit,
                buffer,
            } = self.project();

            if let Some(item) = buffer.remove(commit) {
                *commit += 1;
                cx.waker().clone().wake();
                return Ready(Some(item));
            }

            match stream.poll_next(cx) {
                Ready(Some((index, item))) => match (*commit).cmp(&index) {
                    Less => match buffer.entry(index) {
                        hash_map::Entry::Occupied(_) => {
                            panic!("the index number {} appears more than once", index);
                        }
                        hash_map::Entry::Vacant(entry) => {
                            entry.insert(item);
                            cx.waker().clone().wake();
                            Pending
                        }
                    },
                    Equal => {
                        *commit += 1;
                        cx.waker().clone().wake();
                        Ready(Some(item))
                    }
                    Greater => {
                        panic!("the index number {} appears more than once", index);
                    }
                },
                Ready(None) => {
                    assert!(buffer.is_empty(), "the index numbers are not contiguous");
                    Ready(None)
                }
                Pending => Pending,
            }
        }
    }
}

// par_reduce

pub use par_reduce::*;

mod par_reduce {
    use super::*;

    /// A stream combinator returned from [par_reduce()](ParStreamExt::par_reduce).
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct ParReduce<T> {
        #[derivative(Debug = "ignore")]
        pub(super) future: BoxFuture<'static, Option<T>>,
    }

    impl<T> Future for ParReduce<T> {
        type Output = Option<T>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            Pin::new(&mut self.future).poll(cx)
        }
    }
}

// par_for each

pub use par_for_each::*;

mod par_for_each {
    use super::*;

    /// A stream combinator returned from [par_for_each()](ParStreamExt::par_for_each) and its siblings.
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct ParForEach {
        #[derivative(Debug = "ignore")]
        pub(super) future: BoxFuture<'static, ()>,
    }

    impl Future for ParForEach {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.future).poll(cx)
        }
    }
}

// par_for each_blocking

pub use par_for_each_blocking::*;

mod par_for_each_blocking {
    use super::*;

    /// A stream combinator returned from [par_for_each_blocking()](ParStreamExt::par_for_each_blocking) and its siblings.
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct ParForEachBlocking {
        #[derivative(Debug = "ignore")]
        pub(super) future: BoxFuture<'static, ()>,
    }

    impl Future for ParForEachBlocking {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.future).poll(cx)
        }
    }
}

// batching

pub use batching::*;

mod batching {
    use super::*;

    /// A stream combinator returned from [batching()](ParStreamExt::batching).
    #[derive(Derivative)]
    #[derivative(Debug)]
    pub struct Batching<T> {
        #[derivative(Debug = "ignore")]
        pub(super) stream: BoxStream<'static, T>,
    }

    impl<T> Stream for Batching<T> {
        type Item = T;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.stream).poll_next(cx)
        }
    }
}

// tests

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::{izip, Itertools};
    use rand::prelude::*;
    use std::time::Duration;

    #[tokio::test]
    async fn then_spawned_test() {
        {
            let values: Vec<_> = stream::iter(0..1000)
                .then_spawned(None, |val| async move { val * 2 })
                .collect()
                .await;

            let expect: Vec<_> = (0..1000).map(|val| val * 2).collect();
            assert_eq!(values, expect);
        }
    }

    #[tokio::test]
    async fn map_spawned_test() {
        {
            let values: Vec<_> = stream::iter(0..1000)
                .map_spawned(None, |val| val * 2)
                .collect()
                .await;

            let expect: Vec<_> = (0..1000).map(|val| val * 2).collect();
            assert_eq!(values, expect);
        }
    }

    #[tokio::test]
    async fn broadcast_test() {
        let mut guard = stream::iter(0..).broadcast(2);
        let rx1 = guard.register();
        let rx2 = guard.register();
        guard.finish();

        let (ret1, ret2): (Vec<_>, Vec<_>) =
            futures::join!(rx1.take(100).collect(), rx2.take(100).collect());

        izip!(ret1, 0..100).for_each(|(lhs, rhs)| {
            assert_eq!(lhs, rhs);
        });
        izip!(ret2, 0..100).for_each(|(lhs, rhs)| {
            assert_eq!(lhs, rhs);
        });
    }

    #[tokio::test]
    async fn par_batching_unordered_test() {
        let mut rng = rand::thread_rng();
        let data: Vec<u32> = (0..10000).map(|_| rng.gen_range(0..10)).collect();

        let sums: Vec<_> = stream::iter(data)
            .par_batching_unordered(None, |_, input, output| async move {
                let mut sum = 0;

                while let Ok(val) = input.recv_async().await {
                    let new_sum = sum + val;

                    if new_sum >= 1000 {
                        sum = 0;
                        let result = output.send_async(new_sum).await;
                        if result.is_err() {
                            break;
                        }
                    } else {
                        sum = new_sum
                    }
                }
            })
            .collect()
            .await;

        assert!(sums.iter().all(|&sum| sum >= 1000));
    }

    #[tokio::test]
    async fn batching_test() {
        let sums: Vec<_> = stream::iter(0..10)
            .batching(|input, output| async move {
                let mut sum = 0;

                while let Ok(val) = input.recv_async().await {
                    let new_sum = sum + val;

                    if new_sum >= 10 {
                        sum = 0;

                        let result = output.send_async(new_sum).await;
                        if result.is_err() {
                            break;
                        }
                    } else {
                        sum = new_sum;
                    }
                }
            })
            .collect()
            .await;

        assert_eq!(sums, vec![10, 11, 15]);
    }

    #[tokio::test]
    async fn par_then_output_is_ordered_test() {
        let max = 1000u64;
        stream::iter(0..max)
            .par_then(None, |value| async move {
                rt::sleep(Duration::from_millis(value % 20)).await;
                value
            })
            .fold(0u64, |expect, found| async move {
                assert_eq!(expect, found);
                expect + 1
            })
            .await;
    }

    #[tokio::test]
    async fn par_then_unordered_test() {
        let max = 1000u64;
        let mut values: Vec<_> = stream::iter((0..max).into_iter())
            .par_then_unordered(None, |value| async move {
                rt::sleep(Duration::from_millis(value % 20)).await;
                value
            })
            .collect()
            .await;
        values.sort();
        values.into_iter().fold(0, |expect, found| {
            assert_eq!(expect, found);
            expect + 1
        });
    }

    #[tokio::test]
    async fn par_reduce_test() {
        {
            let sum: Option<u64> = stream::iter(iter::empty())
                .par_reduce(None, |lhs, rhs| async move { lhs + rhs })
                .await;
            assert!(sum.is_none());
        }

        {
            let max = 100_000u64;
            let sum = stream::iter((1..=max).into_iter())
                .par_reduce(None, |lhs, rhs| async move { lhs + rhs })
                .await;
            assert_eq!(sum, Some((1 + max) * max / 2));
        }
    }

    #[tokio::test]
    async fn reorder_index_haling_test() {
        let indexes = vec![5, 2, 1, 0, 6, 4, 3];
        let output: Vec<_> = stream::iter(indexes)
            .then(|index| async move {
                rt::sleep(Duration::from_millis(20)).await;
                (index, index)
            })
            .reorder_enumerated()
            .collect()
            .await;
        assert_eq!(&output, &[0, 1, 2, 3, 4, 5, 6]);
    }

    #[tokio::test]
    async fn enumerate_reorder_test() {
        let max = 1000u64;
        let iterator = (0..max).rev().step_by(2);

        let lhs = stream::iter(iterator.clone())
            .enumerate()
            .par_then_unordered(None, |(index, value)| async move {
                rt::sleep(std::time::Duration::from_millis(value % 20)).await;
                (index, value)
            })
            .reorder_enumerated();
        let rhs = stream::iter(iterator.clone());

        let is_equal =
            async_std::stream::StreamExt::all(&mut lhs.zip(rhs), |(lhs_value, rhs_value)| {
                lhs_value == rhs_value
            })
            .await;
        assert!(is_equal);
    }

    #[tokio::test]
    async fn for_each_test() {
        use std::sync::atomic::{self, AtomicUsize};

        {
            let sum = Arc::new(AtomicUsize::new(0));
            {
                let sum = sum.clone();
                stream::iter(1..=1000)
                    .par_for_each(None, move |value| {
                        let sum = sum.clone();
                        async move {
                            sum.fetch_add(value, atomic::Ordering::SeqCst);
                        }
                    })
                    .await;
            }
            assert_eq!(sum.load(atomic::Ordering::SeqCst), (1 + 1000) * 1000 / 2);
        }

        {
            let sum = Arc::new(AtomicUsize::new(0));
            stream::iter(1..=1000)
                .par_for_each_init(
                    None,
                    || sum.clone(),
                    move |sum, value| {
                        let sum = sum.clone();
                        async move {
                            sum.fetch_add(value, atomic::Ordering::SeqCst);
                        }
                    },
                )
                .await;
            assert_eq!(sum.load(atomic::Ordering::SeqCst), (1 + 1000) * 1000 / 2);
        }
    }

    #[tokio::test]
    async fn tee_halt_test() {
        let mut rx1 = stream::iter(0..).tee(1);
        let mut rx2 = rx1.clone();

        assert!(rx1.next().await.is_some());
        assert!(rx2.next().await.is_some());

        // drop rx1
        drop(rx1);

        // the following should not block
        assert!(rx2.next().await.is_some());
        assert!(rx2.next().await.is_some());
        assert!(rx2.next().await.is_some());
        assert!(rx2.next().await.is_some());
        assert!(rx2.next().await.is_some());
    }

    #[tokio::test]
    async fn tee_test() {
        let orig: Vec<_> = (0..100).collect();

        let rx1 = stream::iter(orig.clone()).tee(1);
        let rx2 = rx1.clone();
        let rx3 = rx1.clone();

        let fut1 = rx1
            .then(|val| async move {
                let millis = rand::thread_rng().gen_range(0..5);
                rt::sleep(Duration::from_millis(millis)).await;
                val
            })
            .collect();
        let fut2 = rx2
            .then(|val| async move {
                let millis = rand::thread_rng().gen_range(0..5);
                rt::sleep(Duration::from_millis(millis)).await;
                val * 2
            })
            .collect();
        let fut3 = rx3
            .then(|val| async move {
                let millis = rand::thread_rng().gen_range(0..5);
                rt::sleep(Duration::from_millis(millis)).await;
                val * 3
            })
            .collect();

        let (vec1, vec2, vec3): (Vec<_>, Vec<_>, Vec<_>) = futures::join!(fut1, fut2, fut3);

        // the collected method is possibly losing some of first few elements
        let start1 = orig.len() - vec1.len();
        let start2 = orig.len() - vec2.len();
        let start3 = orig.len() - vec3.len();

        assert!(orig[start1..]
            .iter()
            .zip(&vec1)
            .all(|(&orig, &val)| orig == val));
        assert!(orig[start2..]
            .iter()
            .zip(&vec2)
            .all(|(&orig, &val)| orig * 2 == val));
        assert!(orig[start3..]
            .iter()
            .zip(&vec3)
            .all(|(&orig, &val)| orig * 3 == val));
    }

    #[tokio::test]
    async fn unfold_blocking_test() {
        {
            let numbers: Vec<_> = super::unfold_blocking(
                None,
                || 0,
                move |count| {
                    let output = count;
                    if output < 1000 {
                        Some((output, count + 1))
                    } else {
                        None
                    }
                },
            )
            .collect()
            .await;

            let expect: Vec<_> = (0..1000).collect();

            assert_eq!(numbers, expect);
        }

        {
            let numbers: Vec<_> = super::unfold_blocking(
                None,
                || (0, rand::thread_rng()),
                move |(acc, mut rng)| {
                    let val = rng.gen_range(1..=10);
                    let acc = acc + val;
                    if acc < 100 {
                        Some((acc, (acc, rng)))
                    } else {
                        None
                    }
                },
            )
            .collect()
            .await;

            assert!(numbers.iter().all(|&val| val < 100));
            assert!(izip!(&numbers, numbers.iter().skip(1)).all(|(&prev, &next)| prev < next));
        }
    }

    #[tokio::test]
    async fn par_unfold_test() {
        let numbers: Vec<_> = super::par_unfold_unordered(
            4,
            |index| async move { (index + 1) * 100 },
            |index, quota| async move {
                (quota > 0).then(|| {
                    let mut rng = rand::thread_rng();
                    let val = rng.gen_range(0..10) + index * 100;
                    (val, quota - 1)
                })
            },
        )
        .collect()
        .await;

        let counts = numbers
            .iter()
            .map(|val| {
                let worker_index = val / 100;
                let number = val - worker_index * 100;
                assert!(number < 10);
                (worker_index, 1)
            })
            .into_grouping_map()
            .sum();

        assert_eq!(counts.len(), 4);
        assert!((0..4).all(|worker_index| counts[&worker_index] == (worker_index + 1) * 100));
    }

    #[tokio::test]
    async fn par_unfold_blocking_test() {
        let numbers: Vec<_> = super::par_unfold_blocking_unordered(
            4,
            |index| {
                let rng = rand::thread_rng();
                let quota = (index + 1) * 100;
                (rng, quota)
            },
            |index, (mut rng, quota)| {
                (quota > 0).then(|| {
                    let val = rng.gen_range(0..10) + index * 100;
                    (val, (rng, quota - 1))
                })
            },
        )
        .collect()
        .await;

        let counts = numbers
            .iter()
            .map(|val| {
                let worker_index = val / 100;
                let number = val - worker_index * 100;
                assert!(number < 10);
                (worker_index, 1)
            })
            .into_grouping_map()
            .sum();

        assert_eq!(counts.len(), 4);
        assert!((0..4).all(|worker_index| counts[&worker_index] == (worker_index + 1) * 100));
    }

    #[tokio::test]
    async fn sync_test() {
        {
            let stream1 = stream::iter([1, 3, 5, 7]);
            let stream2 = stream::iter([2, 4, 6, 8]);

            let collected: Vec<_> = super::sync_by_key(None, |&val| val, [stream1, stream2])
                .collect()
                .await;

            assert_eq!(
                collected,
                [
                    Ok((0, 1)),
                    Ok((1, 2)),
                    Ok((0, 3)),
                    Ok((1, 4)),
                    Ok((0, 5)),
                    Ok((1, 6)),
                    Ok((0, 7)),
                    Ok((1, 8)),
                ]
            );
        }

        {
            let stream1 = stream::iter([1, 2, 3]);
            let stream2 = stream::iter([2, 1, 3]);

            let (synced, leaked): (Vec<_>, Vec<_>) =
                super::sync_by_key(None, |&val| val, [stream1, stream2])
                    .map(|result| match result {
                        Ok(item) => (Some(item), None),
                        Err(item) => (None, Some(item)),
                    })
                    .unzip()
                    .await;
            let synced: Vec<_> = synced.into_iter().flatten().collect();
            let leaked: Vec<_> = leaked.into_iter().flatten().collect();

            assert_eq!(synced, [(0, 1), (0, 2), (1, 2), (0, 3), (1, 3)]);
            assert_eq!(leaked, [(1, 1)]);
        }
    }

    #[tokio::test]
    async fn scan_spawned_test() {
        {
            let collected: Vec<_> = stream::iter([2, 3, 1, 4])
                .scan_spawned(None, 0, |acc, val| async move {
                    let acc = acc + val;
                    Some((acc, acc))
                })
                .collect()
                .await;
            assert_eq!(collected, [2, 5, 6, 10]);
        }

        {
            let collected: Vec<_> = stream::iter([2, 3, 1, 4])
                .scan_spawned(None, 0, |acc, val| async move {
                    let acc = acc + val;
                    (acc != 6).then(|| (acc, acc))
                })
                .collect()
                .await;
            assert_eq!(collected, [2, 5]);
        }
    }
}
