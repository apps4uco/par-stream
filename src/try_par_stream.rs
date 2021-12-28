use crate::{common::*, config::ParParams, rt, utils};
use tokio::sync::broadcast;

/// An extension trait that provides fallible combinators for parallel processing on streams.
pub trait TryParStreamExt
where
    Self: 'static + Send + TryStream,
    Self::Ok: 'static + Send,
    Self::Error: 'static + Send,
{
    /// A fallible analogue to [batching](crate::ParStreamExt::batching) that consumes
    /// as many elements as it likes for each next output element.
    fn try_batching<U, F, Fut>(self, f: F) -> BoxStream<'static, Result<U, Self::Error>>
    where
        U: 'static + Send,
        F: FnOnce(flume::Receiver<Self::Ok>, flume::Sender<U>) -> Fut,
        Fut: 'static + Future<Output = Result<(), Self::Error>> + Send;

    /// A fallible analogue to [par_batching](crate::ParStreamExt::par_batching).
    fn try_par_batching<U, P, F, Fut>(
        self,
        params: P,
        f: F,
    ) -> BoxStream<'static, Result<U, Self::Error>>
    where
        F: FnMut(usize, flume::Receiver<Self::Ok>, flume::Sender<U>) -> Fut,
        Fut: 'static + Future<Output = Result<(), Self::Error>> + Send,
        U: 'static + Send,
        P: Into<ParParams>;

    /// A fallible analogue to [par_then](crate::ParStreamExt::par_then).
    fn try_par_then<U, P, F, Fut>(
        self,
        params: P,
        f: F,
    ) -> BoxStream<'static, Result<U, Self::Error>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(Self::Ok) -> Fut + Send,
        Fut: 'static + Future<Output = Result<U, Self::Error>> + Send;

    /// A fallible analogue to [par_then_unordered](crate::ParStreamExt::par_then_unordered).
    fn try_par_then_unordered<U, P, F, Fut>(
        self,
        params: P,
        f: F,
    ) -> BoxStream<'static, Result<U, Self::Error>>
    where
        U: 'static + Send,
        F: 'static + FnMut(Self::Ok) -> Fut + Send,
        Fut: 'static + Future<Output = Result<U, Self::Error>> + Send,
        P: Into<ParParams>;

    /// A fallible analogue to [par_map](crate::ParStreamExt::par_map).
    fn try_par_map<U, P, F, Func>(
        self,
        params: P,
        f: F,
    ) -> BoxStream<'static, Result<U, Self::Error>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(Self::Ok) -> Func + Send,
        Func: 'static + FnOnce() -> Result<U, Self::Error> + Send;

    /// A fallible analogue to [par_map_unordered](crate::ParStreamExt::par_map_unordered).
    fn try_par_map_unordered<U, P, F, Func>(
        self,
        params: P,
        f: F,
    ) -> BoxStream<'static, Result<U, Self::Error>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(Self::Ok) -> Func + Send,
        Func: 'static + FnOnce() -> Result<U, Self::Error> + Send;

    /// Runs this stream to completion, executing asynchronous closure for each element on the stream
    /// in parallel.
    fn try_par_for_each<P, F, Fut>(
        self,
        params: P,
        f: F,
    ) -> BoxFuture<'static, Result<(), Self::Error>>
    where
        P: Into<ParParams>,
        F: 'static + FnMut(Self::Ok) -> Fut + Send,
        Fut: 'static + Future<Output = Result<(), Self::Error>> + Send;

    /// A fallible analogue to [par_for_each_blocking](crate::ParStreamExt::par_for_each_blocking).
    fn try_par_for_each_blocking<P, F, Func>(
        self,
        params: P,
        f: F,
    ) -> BoxFuture<'static, Result<(), Self::Error>>
    where
        P: Into<ParParams>,
        F: 'static + FnMut(Self::Ok) -> Func + Send,
        Func: 'static + FnOnce() -> Result<(), Self::Error> + Send;
}

impl<S, T, E> TryParStreamExt for S
where
    Self: 'static + Send + Stream<Item = Result<T, E>>,
    T: 'static + Send,
    E: 'static + Send,
{
    fn try_batching<U, F, Fut>(self, f: F) -> BoxStream<'static, Result<U, E>>
    where
        U: 'static + Send,
        F: FnOnce(flume::Receiver<T>, flume::Sender<U>) -> Fut,
        Fut: 'static + Future<Output = Result<(), E>> + Send,
    {
        let mut stream = self.boxed();

        let (input_tx, input_rx) = flume::bounded(0);
        let (output_tx, output_rx) = flume::bounded(0);

        let input_future = async move {
            while let Some(item) = stream.try_next().await? {
                let result = input_tx.send_async(item).await;
                if result.is_err() {
                    break;
                }
            }
            Ok(())
        };
        let batching_future = f(input_rx, output_tx);
        let join_future = future::try_join(input_future, batching_future);

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_future.into_stream().map(|result| result.map(|_| None)),
        )
        .boxed();

        stream::try_unfold(
            (Some(select_stream), None),
            move |(mut stream, error)| async move {
                if let Some(stream_) = &mut stream {
                    match stream_.next().await {
                        Some(Ok(Some(output))) => return Ok(Some((Some(output), (stream, error)))),
                        Some(Ok(None)) => {
                            return Ok(Some((None, (stream, error))));
                        }
                        Some(Err(err)) => {
                            return Ok(Some((None, (stream, Some(err)))));
                        }
                        None => {
                            // stream = None;
                        }
                    }
                }

                if let Some(error) = error {
                    return Err(error);
                }

                Ok(None)
            },
        )
        .try_filter_map(|item| async move { Ok(item) })
        .boxed()
    }

    fn try_par_batching<U, P, F, Fut>(self, params: P, mut f: F) -> BoxStream<'static, Result<U, E>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: FnMut(usize, flume::Receiver<T>, flume::Sender<U>) -> Fut,
        Fut: 'static + Future<Output = Result<(), E>> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();

        let (input_tx, input_rx) = utils::channel(buf_size);
        let (output_tx, output_rx) = utils::channel(buf_size);

        let input_fut = rt::spawn(async move {
            let mut stream = self.boxed();

            while let Some(item) = stream.next().await {
                let result = input_tx.send_async(item?).await;
                if result.is_err() {
                    break;
                }
            }
            Ok(())
        });

        let worker_futs: Vec<_> = (0..num_workers)
            .map(|worker_index| {
                let fut = f(worker_index, input_rx.clone(), output_tx.clone());
                rt::spawn(fut)
            })
            .collect();

        let join_fut = future::try_join(input_fut, future::try_join_all(worker_futs))
            .map(|result| result.map(|_| ()));

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_fut.into_stream().map(|result| result.map(|()| None)),
        )
        .boxed();

        stream::try_unfold(
            (Some(select_stream), None),
            |(mut stream, error)| async move {
                if let Some(stream_) = &mut stream {
                    match stream_.next().await {
                        Some(Ok(Some(item))) => {
                            return Ok(Some((Some(item), (stream, error))));
                        }
                        Some(Ok(None)) => {
                            return Ok(Some((None, (stream, error))));
                        }
                        Some(Err(err)) => {
                            return Ok(Some((None, (stream, Some(err)))));
                        }
                        None => {}
                    }
                }

                if let Some(error) = error {
                    return Err(error);
                }

                Ok(None)
            },
        )
        .try_filter_map(|item| async move { Ok(item) })
        .boxed()
    }

    fn try_par_then<U, P, F, Fut>(self, params: P, mut f: F) -> BoxStream<'static, Result<U, E>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(T) -> Fut + Send,
        Fut: 'static + Future<Output = Result<U, E>> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();

        let (input_tx, input_rx) = utils::channel(buf_size);
        let (reorder_tx, reorder_rx) = utils::channel(buf_size);
        let (output_tx, output_rx) = utils::channel(buf_size);
        let (terminate_tx, mut terminate_rx) = broadcast::channel(1);

        let input_future = {
            rt::spawn(async move {
                let mut stream = self.boxed();
                let mut index = 0;

                loop {
                    let item = tokio::select! {
                        item = stream.try_next() => item.map_err(|err| (index, err))?,
                        _ = terminate_rx.recv() => break,
                    };

                    match item {
                        Some(item) => {
                            let future = f(item);
                            if input_tx.send_async((index, future)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }

                    index += 1;
                }

                Ok(())
            })
        };

        let mut worker_futures: Vec<_> = (0..num_workers)
            .map(|_| {
                let input_rx = input_rx.clone();
                let reorder_tx = reorder_tx.clone();
                let terminate_tx = terminate_tx.clone();

                rt::spawn(async move {
                    loop {
                        let (index, future) = match input_rx.recv_async().await {
                            Ok(item) => item,
                            Err(_) => {
                                break;
                            }
                        };
                        match future.await {
                            Ok(item) => {
                                if reorder_tx.send_async((index, item)).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = terminate_tx.send(());
                                return Err((index, err));
                            }
                        }
                    }

                    Ok(())
                })
                .boxed()
            })
            .collect();

        let select_worker_future = async move {
            let mut errors = vec![];

            while !worker_futures.is_empty() {
                let (result, index, _) = future::select_all(&mut worker_futures).await;
                worker_futures.remove(index);

                if let Err((index, error)) = result {
                    errors.push((index, error));
                }
            }

            errors
        };

        let reorder_future = rt::spawn(async move {
            let mut map = HashMap::new();
            let mut commit = 0;

            'outer: loop {
                let (index, item) = match reorder_rx.recv_async().await {
                    Ok(tuple) => tuple,
                    Err(_) => break,
                };

                match commit.cmp(&index) {
                    Less => {
                        map.insert(index, item);
                    }
                    Equal => {
                        if output_tx.send_async(item).await.is_err() {
                            break 'outer;
                        }
                        commit += 1;

                        'inner: loop {
                            match map.remove(&commit) {
                                Some(item) => {
                                    if output_tx.send_async(item).await.is_err() {
                                        break 'outer;
                                    };
                                    commit += 1;
                                }
                                None => break 'inner,
                            }
                        }
                    }
                    Greater => panic!("duplicated index number {}", index),
                }
            }
        });

        let join_all_future = async move {
            let (input_result, mut worker_results, ()) =
                future::join3(input_future, select_worker_future, reorder_future).await;

            if let Err((_, err)) = input_result {
                return Err(err);
            }

            worker_results.sort_by_cached_key(|&(index, _)| index);
            if let Some((_, err)) = worker_results.into_iter().next() {
                return Err(err);
            }

            Ok(())
        };

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_all_future
                .map(|result| result.map(|()| None))
                .into_stream(),
        )
        .boxed();

        stream::unfold(
            (Some(select_stream), None),
            |(mut select_stream, mut error)| async move {
                if let Some(stream) = &mut select_stream {
                    match stream.next().await {
                        Some(Ok(Some(item))) => {
                            let output = Ok(item);
                            let state = (select_stream, error);
                            return Some((Some(output), state));
                        }
                        Some(Ok(None)) => {
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        Some(Err(err)) => {
                            error = Some(err);
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        None => {
                            // select_stream = None;
                        }
                    }
                }

                if let Some(err) = error {
                    let output = Err(err);
                    let state = (None, None);
                    return Some((Some(output), state));
                }

                None
            },
        )
        .filter_map(|item| async move { item })
        .boxed()
    }

    fn try_par_then_unordered<U, P, F, Fut>(
        self,
        params: P,
        mut f: F,
    ) -> BoxStream<'static, Result<U, E>>
    where
        U: 'static + Send,
        F: 'static + FnMut(T) -> Fut + Send,
        Fut: 'static + Future<Output = Result<U, E>> + Send,
        P: Into<ParParams>,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();
        let (input_tx, input_rx) = utils::channel(buf_size);
        let (output_tx, output_rx) = utils::channel(buf_size);
        let (terminate_tx, mut terminate_rx) = broadcast::channel(1);

        let input_future = {
            async move {
                let mut stream = self.boxed();

                loop {
                    let item = tokio::select! {
                        item = stream.try_next() => item?,
                        _ = terminate_rx.recv() => break
                    };

                    match item {
                        Some(item) => {
                            let fut = f(item);
                            let result = input_tx.send_async(fut).await;
                            if result.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }

                Ok(())
            }
        };

        let mut worker_futures: Vec<_> = (0..num_workers)
            .map(|_| {
                let input_rx = input_rx.clone();
                let output_tx = output_tx.clone();
                let terminate_tx = terminate_tx.clone();

                rt::spawn(async move {
                    loop {
                        let output = match input_rx.recv_async().await {
                            Ok(fut) => fut.await,
                            Err(_) => break,
                        };
                        match output {
                            Ok(output) => {
                                if output_tx.send_async(output).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = terminate_tx.send(());
                                return Err(err);
                            }
                        }
                    }

                    Ok(())
                })
                .boxed()
            })
            .collect();

        let select_worker_future = async move {
            while !worker_futures.is_empty() {
                let (result, index, _) = future::select_all(&mut worker_futures).await;
                worker_futures.remove(index);

                if let Err(error) = result {
                    let _ = future::join_all(worker_futures).await;
                    return Err(error);
                }
            }

            Ok(())
        };

        let join_all_future = async move {
            let (input_result, worker_result) =
                future::join(input_future, select_worker_future).await;

            match (input_result, worker_result) {
                (Err(err), _) => Err(err),
                (Ok(_), Err(err)) => Err(err),
                _ => Ok(()),
            }
        };

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_all_future
                .map(|result| result.map(|()| None))
                .into_stream(),
        )
        .boxed();

        stream::unfold(
            (Some(select_stream), None),
            |(mut select_stream, mut error)| async move {
                if let Some(stream) = &mut select_stream {
                    match stream.next().await {
                        Some(Ok(Some(item))) => {
                            let output = Ok(item);
                            let state = (select_stream, error);
                            return Some((Some(output), state));
                        }
                        Some(Ok(None)) => {
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        Some(Err(err)) => {
                            error = Some(err);
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        None => {
                            // select_stream = None;
                        }
                    }
                }

                if let Some(err) = error {
                    let output = Err(err);
                    let state = (None, None);
                    return Some((Some(output), state));
                }

                None
            },
        )
        .filter_map(|item| async move { item })
        .boxed()
    }

    fn try_par_map<U, P, F, Func>(self, params: P, mut f: F) -> BoxStream<'static, Result<U, E>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(T) -> Func + Send,
        Func: 'static + FnOnce() -> Result<U, E> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();

        let (input_tx, input_rx) = utils::channel(buf_size);
        let (reorder_tx, reorder_rx) = utils::channel(buf_size);
        let (output_tx, output_rx) = utils::channel(buf_size);
        let (terminate_tx, mut terminate_rx) = broadcast::channel(1);

        let input_future = {
            rt::spawn(async move {
                let mut stream = self.boxed();
                let mut index = 0;

                loop {
                    let item = tokio::select! {
                        item = stream.try_next() => item.map_err(|err| (index, err))?,
                        _ = terminate_rx.recv() => break,
                    };

                    match item {
                        Some(item) => {
                            let future = f(item);
                            if input_tx.send_async((index, future)).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }

                    index += 1;
                }

                Ok(())
            })
        };

        let mut worker_futures: Vec<_> = (0..num_workers)
            .map(|_| {
                let input_rx = input_rx.clone();
                let reorder_tx = reorder_tx.clone();
                let terminate_tx = terminate_tx.clone();

                rt::spawn_blocking(move || {
                    loop {
                        let (index, job) = match input_rx.recv() {
                            Ok(item) => item,
                            Err(_) => {
                                break;
                            }
                        };
                        match job() {
                            Ok(item) => {
                                if reorder_tx.send((index, item)).is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = terminate_tx.send(());
                                return Err((index, err));
                            }
                        }
                    }

                    Ok(())
                })
                .boxed()
            })
            .collect();

        let select_worker_future = async move {
            let mut errors = vec![];

            while !worker_futures.is_empty() {
                let (result, index, _) = future::select_all(&mut worker_futures).await;
                worker_futures.remove(index);

                if let Err((index, error)) = result {
                    errors.push((index, error));
                }
            }

            errors
        };

        rt::spawn(async move {
            let mut map = HashMap::new();
            let mut commit = 0;

            'outer: loop {
                let (index, item) = match reorder_rx.recv_async().await {
                    Ok(tuple) => tuple,
                    Err(_) => break,
                };

                match commit.cmp(&index) {
                    Less => {
                        map.insert(index, item);
                    }
                    Equal => {
                        if output_tx.send_async(item).await.is_err() {
                            break 'outer;
                        }
                        commit += 1;

                        'inner: loop {
                            match map.remove(&commit) {
                                Some(item) => {
                                    if output_tx.send_async(item).await.is_err() {
                                        break 'outer;
                                    };
                                    commit += 1;
                                }
                                None => break 'inner,
                            }
                        }
                    }
                    Greater => panic!("duplicated index number {}", index),
                }
            }
        });

        let join_all_future = async move {
            let (input_result, mut worker_results) =
                future::join(input_future, select_worker_future).await;

            if let Err((_, err)) = input_result {
                return Err(err);
            }

            worker_results.sort_by_cached_key(|&(index, _)| index);
            if let Some((_, err)) = worker_results.into_iter().next() {
                return Err(err);
            }

            Ok(())
        };

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_all_future
                .map(|result| result.map(|()| None))
                .into_stream(),
        )
        .boxed();

        stream::unfold(
            (Some(select_stream), None),
            |(mut select_stream, mut error)| async move {
                if let Some(stream) = &mut select_stream {
                    match stream.next().await {
                        Some(Ok(Some(item))) => {
                            let output = Ok(item);
                            let state = (select_stream, error);
                            return Some((Some(output), state));
                        }
                        Some(Ok(None)) => {
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        Some(Err(err)) => {
                            error = Some(err);
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        None => {
                            // select_stream = None;
                        }
                    }
                }

                if let Some(err) = error {
                    let output = Err(err);
                    let state = (None, None);
                    return Some((Some(output), state));
                }

                None
            },
        )
        .filter_map(|item| async move { item })
        .boxed()
    }

    fn try_par_map_unordered<U, P, F, Func>(
        self,
        params: P,
        mut f: F,
    ) -> BoxStream<'static, Result<U, E>>
    where
        P: Into<ParParams>,
        U: 'static + Send,
        F: 'static + FnMut(T) -> Func + Send,
        Func: 'static + FnOnce() -> Result<U, E> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();
        let (input_tx, input_rx) = utils::channel(buf_size);
        let (output_tx, output_rx) = utils::channel(buf_size);
        let (terminate_tx, mut terminate_rx) = broadcast::channel(1);

        let input_future = {
            async move {
                let mut stream = self.boxed();

                loop {
                    let item = tokio::select! {
                        item = stream.try_next() => item?,
                        _ = terminate_rx.recv() => break
                    };

                    match item {
                        Some(item) => {
                            let fut = f(item);
                            let result = input_tx.send_async(fut).await;
                            if result.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }

                Ok(())
            }
        };

        let mut worker_futures: Vec<_> = (0..num_workers)
            .map(|_| {
                let input_rx = input_rx.clone();
                let output_tx = output_tx.clone();
                let terminate_tx = terminate_tx.clone();

                rt::spawn_blocking(move || {
                    loop {
                        let output = match input_rx.recv() {
                            Ok(job) => job(),
                            Err(_) => break,
                        };
                        match output {
                            Ok(output) => {
                                if output_tx.send(output).is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                let _ = terminate_tx.send(());
                                return Err(err);
                            }
                        }
                    }

                    Ok(())
                })
                .boxed()
            })
            .collect();

        let select_worker_future = async move {
            while !worker_futures.is_empty() {
                let (result, index, _) = future::select_all(&mut worker_futures).await;
                worker_futures.remove(index);

                if let Err(error) = result {
                    let _ = future::join_all(worker_futures).await;
                    return Err(error);
                }
            }

            Ok(())
        };

        let join_all_future = async move {
            let (input_result, worker_result) =
                future::join(input_future, select_worker_future).await;

            match (input_result, worker_result) {
                (Err(err), _) => Err(err),
                (Ok(_), Err(err)) => Err(err),
                _ => Ok(()),
            }
        };

        let select_stream = stream::select(
            output_rx.into_stream().map(|item| Ok(Some(item))),
            join_all_future
                .map(|result| result.map(|()| None))
                .into_stream(),
        )
        .boxed();

        stream::unfold(
            (Some(select_stream), None),
            |(mut select_stream, mut error)| async move {
                if let Some(stream) = &mut select_stream {
                    match stream.next().await {
                        Some(Ok(Some(item))) => {
                            let output = Ok(item);
                            let state = (select_stream, error);
                            return Some((Some(output), state));
                        }
                        Some(Ok(None)) => {
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        Some(Err(err)) => {
                            error = Some(err);
                            let state = (select_stream, error);
                            return Some((None, state));
                        }
                        None => {
                            // select_stream = None;
                        }
                    }
                }

                if let Some(err) = error {
                    let output = Err(err);
                    let state = (None, None);
                    return Some((Some(output), state));
                }

                None
            },
        )
        .filter_map(|item| async move { item })
        .boxed()
    }

    fn try_par_for_each<P, F, Fut>(self, params: P, mut f: F) -> BoxFuture<'static, Result<(), E>>
    where
        P: Into<ParParams>,
        F: 'static + FnMut(T) -> Fut + Send,
        Fut: 'static + Future<Output = Result<(), E>> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();
        let (map_tx, map_rx) = utils::channel(buf_size);
        let (terminate_tx, _terminate_rx) = broadcast::channel(1);

        let map_fut = {
            let terminate_tx = terminate_tx.clone();

            async move {
                let mut stream = self.boxed();

                loop {
                    match stream.try_next().await {
                        Ok(Some(item)) => {
                            let fut = f(item);
                            if map_tx.send_async(fut).await.is_err() {
                                break Ok(());
                            }
                        }
                        Ok(None) => break Ok(()),
                        Err(err) => {
                            let _result = terminate_tx.send(()); // shutdown workers
                            break Err(err); // output error
                        }
                    }
                }
            }
        };

        let worker_futs: Vec<_> = (0..num_workers)
            .map(|_| {
                let map_rx = map_rx.clone();
                let terminate_tx = terminate_tx.clone();
                let mut terminate_rx = terminate_tx.subscribe();

                let worker_fut = async move {
                    loop {
                        tokio::select! {
                            result = map_rx.recv_async() => {
                                let fut = match result {
                                    Ok(fut) => fut,
                                    Err(_) => break Ok(()),
                                };

                                if let Err(err) = fut.await {
                                    let _result = terminate_tx.send(()); // shutdown workers
                                    break Err(err); // return error
                                }
                            }
                            _ = terminate_rx.recv() => break Ok(()),
                        }
                    }
                };
                rt::spawn(worker_fut)
            })
            .collect();

        async move {
            let (map_result, worker_results) = join!(map_fut, future::join_all(worker_futs));

            worker_results
                .into_iter()
                .fold(map_result, |folded, result| {
                    // the order takes the latest error
                    result.and(folded)
                })
        }
        .boxed()
    }

    fn try_par_for_each_blocking<P, F, Func>(
        self,
        params: P,
        mut f: F,
    ) -> BoxFuture<'static, Result<(), E>>
    where
        P: Into<ParParams>,
        F: 'static + FnMut(T) -> Func + Send,
        Func: 'static + FnOnce() -> Result<(), E> + Send,
    {
        let ParParams {
            num_workers,
            buf_size,
        } = params.into();
        let (map_tx, map_rx) = utils::channel(buf_size);
        let (terminate_tx, mut terminate_rx) = broadcast::channel(1);

        let input_fut = {
            let terminate_tx = terminate_tx.clone();

            async move {
                let mut stream = self.boxed();

                loop {
                    tokio::select! {
                        item = stream.try_next() => {
                            match item {
                                Ok(Some(item)) => {
                                    let fut = f(item);
                                    if map_tx.send_async(fut).await.is_err() {
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(err) => {
                                    let _ = terminate_tx.send(()); // shutdown workers
                                    return Err(err); // output error
                                }
                            }
                        }
                        _ = terminate_rx.recv() => {
                            break
                        }
                    }
                }

                Ok(())
            }
        };

        let worker_futs: Vec<_> = (0..num_workers)
            .map(|_| {
                let map_rx = map_rx.clone();
                let terminate_tx = terminate_tx.clone();

                rt::spawn_blocking(move || {
                    loop {
                        match map_rx.recv() {
                            Ok(job) => {
                                let result = job();
                                if let Err(err) = result {
                                    let _result = terminate_tx.send(()); // shutdown workers
                                    return Err(err); // return error
                                }
                            }
                            Err(_) => break,
                        }
                    }

                    Ok(())
                })
            })
            .collect();

        async move {
            let (input_result, worker_results) = join!(input_fut, future::join_all(worker_futs));

            worker_results
                .into_iter()
                .fold(input_result, |folded, result| {
                    // the order takes the latest error
                    result.and(folded)
                })
        }
        .boxed()
    }
}

// tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{try_index_stream::TryIndexStreamExt as _, try_stream::TryStreamExt as _};
    use rand::prelude::*;

    #[tokio::test]
    async fn try_par_batching_test() {
        {
            let mut stream = stream::iter(iter::repeat(1).take(10))
                .map(Ok)
                .try_par_batching::<(), _, _, _>(None, |_, _, _| async move {
                    Result::<(), _>::Err("init error")
                });

            assert_eq!(stream.next().await, Some(Err("init error")));
            assert!(stream.next().await.is_none());
        }

        {
            let mut stream = stream::iter(iter::repeat(1).take(10))
                .map(Ok)
                .try_par_batching(None, |_, input, output| async move {
                    let mut sum = 0;

                    while let Ok(val) = input.recv_async().await {
                        let new_sum = sum + val;
                        if new_sum >= 3 {
                            sum = 0;
                            let result = output.send_async(new_sum).await;
                            if result.is_err() {
                                break;
                            }
                        } else {
                            sum = new_sum;
                        }
                    }

                    if sum > 0 {
                        let _ = output.send_async(sum).await;
                    }

                    Result::<_, ()>::Ok(())
                });

            let mut total = 0;
            while total < 10 {
                let sum = stream.next().await.unwrap().unwrap();
                assert!(sum <= 3);
                total += sum;
            }
            assert!(stream.next().await.is_none());
        }

        {
            let mut stream = stream::iter(iter::repeat(1).take(10))
                .map(Ok)
                .try_par_batching(None, |_, input, output| async move {
                    let mut sum = 0;

                    while let Ok(val) = input.recv_async().await {
                        let new_sum = sum + val;
                        if new_sum >= 3 {
                            sum = 0;
                            let result = output.send_async(new_sum).await;
                            if result.is_err() {
                                break;
                            }
                        } else {
                            sum = new_sum;
                        }
                    }

                    if sum == 0 {
                        Ok(())
                    } else {
                        Err(sum)
                    }
                });

            let mut total = 0;
            while total < 10 {
                let result = stream.next().await.unwrap();
                match result {
                    Ok(sum) => {
                        assert!(sum == 3);
                        total += sum;
                    }
                    Err(sum) => {
                        assert!(sum < 3);
                        break;
                    }
                }
            }
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn try_batching_test() {
        {
            let mut stream = stream::iter(0..10)
                .map(Ok)
                .try_batching::<usize, _, _>(|_, _| async move { Err("init error") });

            assert_eq!(stream.next().await, Some(Err("init error")));
            assert!(stream.next().await.is_none());
        }

        {
            let mut stream = stream::iter(0..10)
                .map(Ok)
                .try_batching(|input, output| async move {
                    let mut sum = 0;

                    while let Ok(val) = input.recv_async().await {
                        let new_sum = val + sum;

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

                    if sum == 0 {
                        Ok(())
                    } else {
                        dbg!();
                        Err("some elements are left behind")
                    }
                });

            assert_eq!(stream.next().await, Some(Ok(10)));
            assert_eq!(stream.next().await, Some(Ok(11)));
            assert_eq!(stream.next().await, Some(Ok(15)));
            assert!(matches!(stream.next().await, Some(Err(_))));
            assert!(stream.next().await.is_none());
        }

        {
            let mut stream = stream::iter(0..10)
                .map(Ok)
                .try_batching(|input, output| async move {
                    let mut sum = 0;

                    while let Ok(val) = input.recv_async().await {
                        let new_sum = val + sum;

                        if new_sum >= 15 {
                            return Err("too large");
                        } else if new_sum >= 10 {
                            sum = 0;
                            let result = output.send_async(new_sum).await;
                            if result.is_err() {
                                break;
                            }
                        } else {
                            sum = new_sum;
                        }
                    }

                    if input.recv_async().await.is_err() {
                        Ok(())
                    } else {
                        Err("some elements are left behind")
                    }
                });

            assert_eq!(stream.next().await, Some(Ok(10)));
            assert_eq!(stream.next().await, Some(Ok(11)));
            assert_eq!(stream.next().await, Some(Err("too large")));
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn try_par_for_each_test() {
        {
            let result = stream::iter(vec![Ok(1usize), Ok(2), Ok(6), Ok(4)].into_iter())
                .try_par_for_each(None, |_| async move { Result::<_, ()>::Ok(()) })
                .await;

            assert_eq!(result, Ok(()));
        }

        {
            let result = stream::iter(vec![Ok(1usize), Ok(2), Err(-3isize), Ok(4)].into_iter())
                .try_par_for_each(None, |_| async move { Ok(()) })
                .await;

            assert_eq!(result, Err(-3));
        }
    }

    #[tokio::test]
    async fn try_par_for_each_blocking_test() {
        {
            let result = stream::iter(vec![Ok(1usize), Ok(2), Ok(6), Ok(4)])
                .try_par_for_each_blocking(None, |_| || Result::<_, ()>::Ok(()))
                .await;

            assert_eq!(result, Ok(()));
        }

        {
            let result = stream::iter(0..)
                .then(|val| async move {
                    if val == 3 {
                        Err(val)
                    } else {
                        Ok(val)
                    }
                })
                .try_par_for_each_blocking(8, |_| || Ok(()))
                .await;

            assert_eq!(result, Err(3));
        }

        {
            let result = stream::iter(0..)
                .map(Ok)
                .try_par_for_each_blocking(None, |val| {
                    move || {
                        if val == 3 {
                            std::thread::sleep(Duration::from_millis(100));
                            Err(val)
                        } else {
                            Ok(())
                        }
                    }
                })
                .await;

            assert_eq!(result, Err(3));
        }
    }

    #[tokio::test]
    async fn try_par_then_test() {
        {
            let mut stream = stream::iter(vec![Ok(1usize), Ok(2), Err(-3isize), Ok(4)].into_iter())
                .try_par_then(None, |value| async move { Ok(value) });

            assert_eq!(stream.try_next().await, Ok(Some(1usize)));
            assert_eq!(stream.try_next().await, Ok(Some(2usize)));
            assert_eq!(stream.try_next().await, Err(-3isize));
            assert_eq!(stream.try_next().await, Ok(None));
        }

        {
            let vec: Result<Vec<()>, ()> = stream::iter(vec![])
                .try_par_then(None, |()| async move { Ok(()) })
                .try_collect()
                .await;

            assert!(matches!(vec, Ok(vec) if vec.is_empty()));
        }

        {
            let mut stream =
                stream::repeat(())
                    .enumerate()
                    .map(Ok)
                    .try_par_then(3, |(index, ())| async move {
                        match index {
                            3 | 6 => Err(index),
                            index => Ok(index),
                        }
                    });

            assert_eq!(stream.next().await, Some(Ok(0)));
            assert_eq!(stream.next().await, Some(Ok(1)));
            assert_eq!(stream.next().await, Some(Ok(2)));
            assert_eq!(stream.next().await, Some(Err(3)));
            assert!(stream.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn try_reorder_enumerated_test() {
        let len: usize = 1000;
        let mut rng = rand::thread_rng();

        for _ in 0..10 {
            let err_index_1 = rng.gen_range(0..len);
            let err_index_2 = rng.gen_range(0..len);
            let min_err_index = err_index_1.min(err_index_2);

            let results: Vec<_> = stream::iter(0..len)
                .map(move |value| {
                    if value == err_index_1 || value == err_index_2 {
                        Err(-(value as isize))
                    } else {
                        Ok(value)
                    }
                })
                .try_enumerate()
                .try_par_then_unordered(None, |(index, value)| async move {
                    rt::sleep(Duration::from_millis(value as u64 % 10)).await;
                    Ok((index, value))
                })
                .try_reorder_enumerated()
                .collect()
                .await;
            assert!(results.len() <= min_err_index + 1);

            let (is_fused_at_error, _, _) = results.iter().cloned().fold(
                (true, false, 0),
                |(is_correct, found_err, expect), result| {
                    if !is_correct {
                        return (false, found_err, expect);
                    }

                    match result {
                        Ok(value) => {
                            let is_correct = value < min_err_index && value == expect && !found_err;
                            (is_correct, found_err, expect + 1)
                        }
                        Err(value) => {
                            let is_correct = (-value) as usize == min_err_index && !found_err;
                            let found_err = true;
                            (is_correct, found_err, expect + 1)
                        }
                    }
                },
            );
            assert!(is_fused_at_error);
        }
    }
}
