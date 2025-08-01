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

use std::sync::Arc;

use futures::select;
use futures::Future;
use futures::FutureExt;

use crate::raw::*;
use crate::*;

/// MultipartWrite is used to implement [`oio::Write`] based on multipart
/// uploads. By implementing MultipartWrite, services don't need to
/// care about the details of uploading parts.
///
/// # Architecture
///
/// The architecture after adopting [`MultipartWrite`]:
///
/// - Services impl `MultipartWrite`
/// - `MultipartWriter` impl `Write`
/// - Expose `MultipartWriter` as `Accessor::Writer`
///
/// # Notes
///
/// `MultipartWrite` has an oneshot optimization when `write` has been called only once:
///
/// ```no_build
/// w.write(bs).await?;
/// w.close().await?;
/// ```
///
/// We will use `write_once` instead of starting a new multipart upload.
///
/// # Requirements
///
/// Services that implement `BlockWrite` must fulfill the following requirements:
///
/// - Must be a http service that could accept `AsyncBody`.
/// - Don't need initialization before writing.
/// - Block ID is generated by caller `BlockWrite` instead of services.
/// - Complete block by an ordered block id list.
pub trait MultipartWrite: Send + Sync + Unpin + 'static {
    /// write_once is used to write the data to underlying storage at once.
    ///
    /// MultipartWriter will call this API when:
    ///
    /// - All the data has been written to the buffer and we can perform the upload at once.
    fn write_once(
        &self,
        size: u64,
        body: Buffer,
    ) -> impl Future<Output = Result<Metadata>> + MaybeSend;

    /// initiate_part will call start a multipart upload and return the upload id.
    ///
    /// MultipartWriter will call this when:
    ///
    /// - the total size of data is unknown.
    /// - the total size of data is known, but the size of current write
    ///   is less than the total size.
    fn initiate_part(&self) -> impl Future<Output = Result<String>> + MaybeSend;

    /// write_part will write a part of the data and returns the result
    /// [`MultipartPart`].
    ///
    /// MultipartWriter will call this API and stores the result in
    /// order.
    ///
    /// - part_number is the index of the part, starting from 0.
    fn write_part(
        &self,
        upload_id: &str,
        part_number: usize,
        size: u64,
        body: Buffer,
    ) -> impl Future<Output = Result<MultipartPart>> + MaybeSend;

    /// complete_part will complete the multipart upload to build the final
    /// file.
    fn complete_part(
        &self,
        upload_id: &str,
        parts: &[MultipartPart],
    ) -> impl Future<Output = Result<Metadata>> + MaybeSend;

    /// abort_part will cancel the multipart upload and purge all data.
    fn abort_part(&self, upload_id: &str) -> impl Future<Output = Result<()>> + MaybeSend;
}

/// The result of [`MultipartWrite::write_part`].
///
/// services implement should convert MultipartPart to their own represents.
///
/// - `part_number` is the index of the part, starting from 0.
/// - `etag` is the `ETag` of the part.
/// - `checksum` is the optional checksum of the part.
#[derive(Clone)]
pub struct MultipartPart {
    /// The number of the part, starting from 0.
    pub part_number: usize,
    /// The etag of the part.
    pub etag: String,
    /// The checksum of the part.
    pub checksum: Option<String>,
}

struct WriteInput<W: MultipartWrite> {
    w: Arc<W>,
    executor: Executor,
    upload_id: Arc<String>,
    part_number: usize,
    bytes: Buffer,
}

/// MultipartWriter will implement [`oio::Write`] based on multipart
/// uploads.
pub struct MultipartWriter<W: MultipartWrite> {
    w: Arc<W>,
    executor: Executor,

    upload_id: Option<Arc<String>>,
    parts: Vec<MultipartPart>,
    cache: Option<Buffer>,
    next_part_number: usize,

    tasks: ConcurrentTasks<WriteInput<W>, MultipartPart>,
}

/// # Safety
///
/// wasm32 is a special target that we only have one event-loop for this state.
impl<W: MultipartWrite> MultipartWriter<W> {
    /// Create a new MultipartWriter.
    pub fn new(info: Arc<AccessorInfo>, inner: W, concurrent: usize) -> Self {
        let w = Arc::new(inner);
        let executor = info.executor();
        Self {
            w,
            executor: executor.clone(),
            upload_id: None,
            parts: Vec::new(),
            cache: None,
            next_part_number: 0,

            tasks: ConcurrentTasks::new(executor, concurrent, 8192, |input| {
                Box::pin({
                    async move {
                        let fut = input.w.write_part(
                            &input.upload_id,
                            input.part_number,
                            input.bytes.len() as u64,
                            input.bytes.clone(),
                        );
                        match input.executor.timeout() {
                            None => {
                                let result = fut.await;
                                (input, result)
                            }
                            Some(timeout) => {
                                let result = select! {
                                    result = fut.fuse() => {
                                        result
                                    }
                                    _ = timeout.fuse() => {
                                        Err(Error::new(
                                            ErrorKind::Unexpected, "write part timeout")
                                                .with_context("upload_id", input.upload_id.to_string())
                                                .with_context("part_number", input.part_number.to_string())
                                                .set_temporary())
                                    }
                                };
                                (input, result)
                            }
                        }
                    }
                })
            }),
        }
    }

    fn fill_cache(&mut self, bs: Buffer) -> usize {
        let size = bs.len();
        assert!(self.cache.is_none());
        self.cache = Some(bs);
        size
    }
}

impl<W> oio::Write for MultipartWriter<W>
where
    W: MultipartWrite,
{
    async fn write(&mut self, bs: Buffer) -> Result<()> {
        let upload_id = match self.upload_id.clone() {
            Some(v) => v,
            None => {
                // Fill cache with the first write.
                if self.cache.is_none() {
                    self.fill_cache(bs);
                    return Ok(());
                }

                let upload_id = self.w.initiate_part().await?;
                let upload_id = Arc::new(upload_id);
                self.upload_id = Some(upload_id.clone());
                upload_id
            }
        };

        let bytes = self.cache.clone().expect("pending write must exist");
        let part_number = self.next_part_number;

        self.tasks
            .execute(WriteInput {
                w: self.w.clone(),
                executor: self.executor.clone(),
                upload_id: upload_id.clone(),
                part_number,
                bytes,
            })
            .await?;
        self.cache = None;
        self.next_part_number += 1;
        self.fill_cache(bs);
        Ok(())
    }

    async fn close(&mut self) -> Result<Metadata> {
        let upload_id = match self.upload_id.clone() {
            Some(v) => v,
            None => {
                let (size, body) = match self.cache.clone() {
                    Some(cache) => (cache.len(), cache),
                    None => (0, Buffer::new()),
                };

                // Call write_once if there is no upload_id.
                let meta = self.w.write_once(size as u64, body).await?;
                // make sure to clear the cache only after write_once succeeds; otherwise, retries may fail.
                self.cache = None;
                return Ok(meta);
            }
        };

        if let Some(cache) = self.cache.clone() {
            let part_number = self.next_part_number;

            self.tasks
                .execute(WriteInput {
                    w: self.w.clone(),
                    executor: self.executor.clone(),
                    upload_id: upload_id.clone(),
                    part_number,
                    bytes: cache,
                })
                .await?;
            self.cache = None;
            self.next_part_number += 1;
        }

        loop {
            let Some(result) = self.tasks.next().await.transpose()? else {
                break;
            };
            self.parts.push(result)
        }

        if self.parts.len() != self.next_part_number {
            return Err(Error::new(
                ErrorKind::Unexpected,
                "multipart part numbers mismatch, please report bug to opendal",
            )
            .with_context("expected", self.next_part_number)
            .with_context("actual", self.parts.len())
            .with_context("upload_id", upload_id));
        }
        self.w.complete_part(&upload_id, &self.parts).await
    }

    async fn abort(&mut self) -> Result<()> {
        let Some(upload_id) = self.upload_id.clone() else {
            return Ok(());
        };

        self.tasks.clear();
        self.cache = None;
        self.w.abort_part(&upload_id).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use rand::thread_rng;
    use rand::Rng;
    use rand::RngCore;
    use tokio::sync::Mutex;
    use tokio::time::sleep;
    use tokio::time::timeout;

    use super::*;
    use crate::raw::oio::Write;

    struct TestWrite {
        upload_id: String,
        part_numbers: Vec<usize>,
        length: u64,
        content: Option<Buffer>,
    }

    impl TestWrite {
        pub fn new() -> Arc<Mutex<Self>> {
            let v = Self {
                upload_id: uuid::Uuid::new_v4().to_string(),
                part_numbers: Vec::new(),
                length: 0,
                content: None,
            };

            Arc::new(Mutex::new(v))
        }
    }

    impl MultipartWrite for Arc<Mutex<TestWrite>> {
        async fn write_once(&self, size: u64, body: Buffer) -> Result<Metadata> {
            sleep(Duration::from_nanos(50)).await;

            if thread_rng().gen_bool(1.0 / 10.0) {
                return Err(
                    Error::new(ErrorKind::Unexpected, "I'm a crazy monkey!").set_temporary()
                );
            }

            let mut this = self.lock().await;
            this.length = size;
            this.content = Some(body);
            Ok(Metadata::default().with_content_length(size))
        }

        async fn initiate_part(&self) -> Result<String> {
            let upload_id = self.lock().await.upload_id.clone();
            Ok(upload_id)
        }

        async fn write_part(
            &self,
            upload_id: &str,
            part_number: usize,
            size: u64,
            _: Buffer,
        ) -> Result<MultipartPart> {
            {
                let test = self.lock().await;
                assert_eq!(upload_id, test.upload_id);
            }

            // Add an async sleep here to enforce some pending.
            sleep(Duration::from_nanos(50)).await;

            // We will have 10% percent rate for write part to fail.
            if thread_rng().gen_bool(1.0 / 10.0) {
                return Err(
                    Error::new(ErrorKind::Unexpected, "I'm a crazy monkey!").set_temporary()
                );
            }

            {
                let mut test = self.lock().await;
                test.part_numbers.push(part_number);
                test.length += size;
            }

            Ok(MultipartPart {
                part_number,
                etag: "etag".to_string(),
                checksum: None,
            })
        }

        async fn complete_part(
            &self,
            upload_id: &str,
            parts: &[MultipartPart],
        ) -> Result<Metadata> {
            let test = self.lock().await;
            assert_eq!(upload_id, test.upload_id);
            assert_eq!(parts.len(), test.part_numbers.len());

            Ok(Metadata::default().with_content_length(test.length))
        }

        async fn abort_part(&self, upload_id: &str) -> Result<()> {
            let test = self.lock().await;
            assert_eq!(upload_id, test.upload_id);

            Ok(())
        }
    }

    struct TimeoutExecutor {
        exec: Arc<dyn Execute>,
    }

    impl TimeoutExecutor {
        pub fn new() -> Self {
            Self {
                exec: Executor::new().into_inner(),
            }
        }
    }

    impl Execute for TimeoutExecutor {
        fn execute(&self, f: BoxedStaticFuture<()>) {
            self.exec.execute(f)
        }

        fn timeout(&self) -> Option<BoxedStaticFuture<()>> {
            let time = thread_rng().gen_range(0..100);
            Some(Box::pin(tokio::time::sleep(Duration::from_nanos(time))))
        }
    }

    #[tokio::test]
    async fn test_multipart_upload_writer_with_concurrent_errors() {
        let mut rng = thread_rng();

        let info = Arc::new(AccessorInfo::default());
        info.update_executor(|_| Executor::with(TimeoutExecutor::new()));

        let mut w = MultipartWriter::new(info, TestWrite::new(), 200);
        let mut total_size = 0u64;

        for _ in 0..1000 {
            let size = rng.gen_range(1..1024);
            total_size += size as u64;

            let mut bs = vec![0; size];
            rng.fill_bytes(&mut bs);

            loop {
                match timeout(Duration::from_nanos(10), w.write(bs.clone().into())).await {
                    Ok(Ok(_)) => break,
                    Ok(Err(_)) => continue,
                    Err(_) => {
                        continue;
                    }
                }
            }
        }

        loop {
            match timeout(Duration::from_nanos(10), w.close()).await {
                Ok(Ok(_)) => break,
                Ok(Err(_)) => continue,
                Err(_) => {
                    continue;
                }
            }
        }

        let actual_parts: Vec<_> = w.parts.into_iter().map(|v| v.part_number).collect();
        let expected_parts: Vec<_> = (0..1000).collect();
        assert_eq!(actual_parts, expected_parts);

        let actual_size = w.w.lock().await.length;
        assert_eq!(actual_size, total_size);
    }

    #[tokio::test]
    async fn test_multipart_writer_with_retry_when_write_once_error() {
        let mut rng = thread_rng();

        for _ in 0..100 {
            let mut w = MultipartWriter::new(Arc::default(), TestWrite::new(), 200);
            let size = rng.gen_range(1..1024);
            let mut bs = vec![0; size];
            rng.fill_bytes(&mut bs);

            loop {
                match w.write(bs.clone().into()).await {
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }

            loop {
                match w.close().await {
                    Ok(_) => break,
                    Err(_) => continue,
                }
            }

            let inner = w.w.lock().await;
            assert_eq!(inner.length, size as u64);
            assert!(inner.content.is_some());
            assert_eq!(inner.content.clone().unwrap().to_bytes(), bs);
        }
    }
}
