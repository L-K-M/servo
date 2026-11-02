/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use embedder_traits::{StopLoadingError, StopLoadingId};
use rustc_hash::FxHashMap;
use servo_base::id::{PipelineId, WebViewId};
use servo_constellation_traits::EmbedderToConstellationMessage;

use crate::proxies::ConstellationProxy;

pub(crate) type StopLoadingCallback = Box<dyn FnOnce(Result<PipelineId, StopLoadingError>)>;

/// A struct that tracks ongoing `WebView::stop_loading` requests and is responsible
/// for calling their callbacks once the `Constellation` reports the result. The
/// callbacks always run on the embedder event loop that owns this `Servo` instance.
pub(crate) struct StopLoadingHandler {
    current_id: StopLoadingId,
    constellation_proxy: ConstellationProxy,
    pending_stops: FxHashMap<StopLoadingId, StopLoadingCallback>,
}

impl StopLoadingHandler {
    pub(crate) fn new(constellation_proxy: ConstellationProxy) -> Self {
        Self {
            current_id: StopLoadingId(0),
            constellation_proxy,
            pending_stops: Default::default(),
        }
    }

    fn generate_id(&mut self) -> StopLoadingId {
        let next_id = StopLoadingId(self.current_id.0 + 1);
        std::mem::replace(&mut self.current_id, next_id)
    }

    pub(crate) fn stop_loading(
        &mut self,
        webview_id: WebViewId,
        callback: StopLoadingCallback,
    ) -> Result<(), StopLoadingError> {
        let stop_loading_id = self.generate_id();
        self.constellation_proxy
            .try_send(EmbedderToConstellationMessage::StopLoading(
                webview_id,
                stop_loading_id,
            ))
            .map_err(|_| StopLoadingError::ChannelClosed)?;
        self.pending_stops.insert(stop_loading_id, callback);
        Ok(())
    }

    pub(crate) fn finish_stop_loading(
        &mut self,
        stop_loading_id: StopLoadingId,
        result: Result<PipelineId, StopLoadingError>,
    ) {
        (self
            .pending_stops
            .remove(&stop_loading_id)
            .expect("Received result for unknown stop loading request."))(result)
    }
}
