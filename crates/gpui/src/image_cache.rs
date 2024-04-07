use crate::{AppContext, ImageData, ImageId, SharedUri, Task};
use collections::HashMap;
use futures::{future::Shared, FutureExt, TryFutureExt};
use image::ImageError;
use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

pub use image::ImageFormat;

#[derive(PartialEq, Eq, Hash, Clone)]
pub(crate) struct RenderImageParams {
    pub(crate) image_id: ImageId,
}

#[derive(Debug, Error, Clone)]
pub(crate) enum Error {
    #[error("IO error: {0}")]
    Io(Arc<std::io::Error>),
    #[error("image error: {0}")]
    Image(Arc<ImageError>),
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(Arc::new(error))
    }
}

impl From<ImageError> for Error {
    fn from(error: ImageError) -> Self {
        Error::Image(Arc::new(error))
    }
}

pub(crate) struct ImageCache {
    images: Arc<Mutex<HashMap<UriOrPath, FetchImageTask>>>,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub(crate) enum UriOrPath {
    Uri(SharedUri),
    Path(Arc<PathBuf>),
}

impl From<SharedUri> for UriOrPath {
    fn from(value: SharedUri) -> Self {
        Self::Uri(value)
    }
}

impl From<Arc<PathBuf>> for UriOrPath {
    fn from(value: Arc<PathBuf>) -> Self {
        Self::Path(value)
    }
}

type FetchImageTask = Shared<Task<Result<Arc<ImageData>, Error>>>;

impl ImageCache {
    pub fn new() -> Self {
        ImageCache {
            images: Default::default(),
        }
    }

    pub fn get(&self, uri_or_path: impl Into<UriOrPath>, cx: &AppContext) -> FetchImageTask {
        let uri_or_path = uri_or_path.into();
        let mut images = self.images.lock();

        match images.get(&uri_or_path) {
            Some(future) => future.clone(),
            None => {
                let future = cx
                    .background_executor()
                    .spawn(
                        {
                            let uri_or_path = uri_or_path.clone();
                            async move {
                                match uri_or_path {
                                    UriOrPath::Path(uri) => {
                                        let image = image::open(uri.as_ref())?.into_bgra8();
                                        Ok(Arc::new(ImageData::new(image)))
                                    }
                                    UriOrPath::Uri(_uri) => {
                                        println!("fetching images is disabled");
                                        let body = Vec::new();
                                        let format = image::guess_format(&body)?;
                                        let image =
                                            image::load_from_memory_with_format(&body, format)?
                                                .into_bgra8();
                                        Ok(Arc::new(ImageData::new(image)))
                                    }
                                }
                            }
                        }
                        .map_err({
                            let uri_or_path = uri_or_path.clone();
                            move |error| {
                                log::log!(log::Level::Error, "{:?} {:?}", &uri_or_path, &error);
                                error
                            }
                        }),
                    )
                    .shared();

                images.insert(uri_or_path, future.clone());
                future
            }
        }
    }
}
