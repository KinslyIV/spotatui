use anyhow::anyhow;
use log::{debug, info, warn};
use ratatui::{
  layout::{Rect, Size},
  Frame,
};
use ratatui_image::{
  picker::{Picker, ProtocolType},
  protocol::StatefulProtocol,
  Resize, StatefulImage,
};
use std::sync::Mutex;

pub struct CoverArt {
  pub state: Mutex<Option<CoverArtState>>,
  /// Separate protocol state for fullscreen cover art view, avoiding conflicts
  /// when the same image is rendered in both the playbar and fullscreen in one frame.
  pub fullscreen_state: Mutex<Option<CoverArtState>>,
  picker: Picker,
}

pub struct CoverArtState {
  url: String,
  image: StatefulProtocol,
}

impl CoverArtState {
  fn new(url: String, image: StatefulProtocol) -> Self {
    Self { url, image }
  }
}

impl CoverArt {
  pub fn new() -> Self {
    let picker = Picker::from_query_stdio().unwrap_or_else(|err| {
      warn!("cover art renderer fallback to halfblocks: {err}");
      Picker::halfblocks()
    });

    info!(
      "cover art rendered detected a {:?} backend",
      picker.protocol_type()
    );
    Self {
      picker,
      state: Mutex::new(None),
      fullscreen_state: Mutex::new(None),
    }
  }

  pub fn full_image_support(&self) -> bool {
    match self.picker.protocol_type() {
      ProtocolType::Kitty | ProtocolType::Iterm2 | ProtocolType::Sixel => true,
      ProtocolType::Halfblocks => false,
    }
  }

  pub fn get_url(&self) -> Option<String> {
    self.state.lock().unwrap().as_ref().map(|s| s.url.clone())
  }

  pub fn set_state(&self, state: CoverArtState) {
    let mut lock = self.state.lock().unwrap();
    *lock = Some(state);
  }

  /// Downloads and decodes the cover-art image at `url`, off any external lock.
  ///
  /// This is a **self-free associated function** on purpose: it borrows neither
  /// `self` nor any `App` state, so its `.await` points can be reached with the
  /// `App` mutex fully dropped. The network fetch and the (synchronous, CPU-bound)
  /// image decode are the expensive parts and must never run while the `App`
  /// guard is held, or the render loop — which locks the same mutex every frame —
  /// freezes for the whole CDN round-trip (#142).
  ///
  /// The reqwest client is built with explicit timeouts so a hung CDN cannot
  /// stall the fetch forever even off-lock (`reqwest::get` uses a default client
  /// with none).
  pub async fn fetch_and_decode(url: &str) -> anyhow::Result<image::DynamicImage> {
    info!("getting new cover art image...");

    let client = reqwest::Client::builder()
      .connect_timeout(std::time::Duration::from_secs(10))
      .timeout(std::time::Duration::from_secs(30))
      .build()
      .map_err(|e| anyhow!(e))?;

    let res = client
      .get(url)
      .send()
      .await
      .and_then(|r| r.error_for_status());

    let file = match res {
      Ok(res) => {
        // Allocate Vec "file" with capacity if content_length is provided
        let mut file = match res.content_length() {
          Some(s) => Vec::with_capacity(s as usize),
          None => Vec::new(),
        };

        let bytes = res.bytes().await?;
        file.extend_from_slice(&bytes);

        debug!("finished reading response: {} bytes", file.len());
        file
      }
      Err(e) => return Err(anyhow!(e)),
    };

    image::load_from_memory(&file).map_err(|e| anyhow!(e))
  }

  /// Stores an already-decoded cover-art image into playbar + fullscreen state.
  ///
  /// Synchronous and cheap: `new_resize_protocol` defers the actual encoding to
  /// render time. Call this under the (briefly re-acquired) `App` guard after
  /// [`fetch_and_decode`] has done the slow work off-lock.
  pub fn store_decoded(&self, url: String, img: image::DynamicImage) {
    // Create two separate protocol instances so the playbar and fullscreen
    // views can render independently without conflicting.
    let image_protocol = self.picker.new_resize_protocol(img.clone());
    let fullscreen_protocol = self.picker.new_resize_protocol(img);

    self.set_state(CoverArtState::new(url.clone(), image_protocol));
    {
      let mut lock = self.fullscreen_state.lock().unwrap();
      *lock = Some(CoverArtState::new(url.clone(), fullscreen_protocol));
    }
    info!("got new cover art: {url}");
  }

  pub fn available(&self) -> bool {
    self.state.lock().unwrap().is_some()
  }

  pub fn render(&self, f: &mut Frame, area: Rect) {
    Self::render_state(&self.state, f, area);
  }

  pub fn size_for(&self, area: Rect) -> Option<Rect> {
    Self::size_for_state(&self.state, area)
  }

  pub fn render_fullscreen(&self, f: &mut Frame, area: Rect) {
    Self::render_state(&self.fullscreen_state, f, area);
  }

  pub fn fullscreen_size_for(&self, area: Rect) -> Option<Rect> {
    Self::size_for_state(&self.fullscreen_state, area)
  }

  fn render_state(state: &Mutex<Option<CoverArtState>>, f: &mut Frame, area: Rect) {
    let mut lock = state.lock().unwrap();
    if let Some(sp) = lock.as_mut() {
      f.render_stateful_widget(
        StatefulImage::new().resize(Resize::Fit(None)),
        area,
        &mut sp.image,
      );
    }
  }

  fn size_for_state(state: &Mutex<Option<CoverArtState>>, area: Rect) -> Option<Rect> {
    let lock = state.lock().unwrap();
    lock.as_ref().map(|sp| {
      let size = sp.image.size_for(
        Resize::Fit(None),
        Size {
          width: area.width,
          height: area.height,
        },
      );
      Rect::new(0, 0, size.width, size.height)
    })
  }
}
