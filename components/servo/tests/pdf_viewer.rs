/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Combray's PDF viewer document (see `DOWNSTREAM.md`).
//!
//! A top-level `application/pdf` response becomes a document with an opaque origin that
//! loads the embedder's viewer module under the embedder's Content-Security-Policy, and the
//! body reaches the viewer through a `blob:` URL. A test protocol handler stands in for the
//! embedder's `combray-pdf:` scheme. The viewer modules report through `document.title`.
//!
//! Servo's global options can be set only once per process, so every test runs alone in a
//! child process of this test binary, and `cargo test --test pdf_viewer` runs the suite.

mod common;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::{Future, ready};
use std::pin::Pin;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use http::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE};
use http::{HeaderName, HeaderValue, Method};
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::{Request as HyperRequest, Response as HyperResponse};
use net::test_util::{Server, make_body, make_server, replace_host_table};
use servo::profile_traits::mem::MemoryReportResult;
use servo::protocol_handler::{
    DoneChannel, FetchContext, NetworkError, ProtocolHandler, ProtocolRegistry, Request,
    ResourceFetchTiming, Response, ResponseBody,
};
use servo::{
    CreateNewWebViewRequest, JSValue, LoadStatus, MouseButton, RenderingContext, WebView,
    WebViewBuilder, WebViewDelegate,
};
use servo_base::generic_channel::GenericCallback;
use servo_config::prefs::{self, Preferences};
use url::Url;
use webrender_api::units::DevicePoint;

use crate::common::{
    ServoTest, click_at_point, evaluate_javascript, wait_for_webview_scene_to_be_up_to_date,
};

const VIEWER_HOST: &str = "viewer-test";
const VIEWER_ORIGIN: &str = "combray-pdf://viewer-test/";
const CHILD_PROCESS_VARIABLE: &str = "SERVO_PDF_VIEWER_TEST_CHILD";
const TIMEOUT: Duration = Duration::from_secs(60);
/// Long enough to arrive in many network chunks.
const PDF_LENGTH: usize = 300 * 1024;
const LARGE_PDF_LENGTH: usize = 64 * 1024 * 1024;
const SMALL_LIMIT: u64 = 1024 * 1024;

/// Run `test` alone in a child process of this test binary, unless this is that process.
fn run_in_own_process(name: &str, test: fn()) {
    if std::env::var_os(CHILD_PROCESS_VARIABLE).is_some() {
        test();
        return;
    }

    let output = Command::new(std::env::current_exe().expect("Should know the test binary"))
        .args([name, "--exact", "--test-threads=1", "--nocapture"])
        .env(CHILD_PROCESS_VARIABLE, "1")
        .output()
        .expect("Should be able to run the test in a child process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "{name} failed: {}", output.status);
    assert!(stdout.contains("1 passed"), "{name} did not run");
}

macro_rules! pdf_viewer_test {
    ($(#[$attribute:meta])* fn $name:ident() $body:block) => {
        $(#[$attribute])*
        #[test]
        fn $name() {
            fn test() $body
            run_in_own_process(stringify!($name), test);
        }
    };
}

/// The viewer's policy, as the embedder sets it.
fn viewer_csp() -> String {
    format!(
        "default-src 'none'; script-src {VIEWER_ORIGIN} 'wasm-unsafe-eval'; \
         worker-src {VIEWER_ORIGIN}; connect-src {VIEWER_ORIGIN} blob:; \
         style-src {VIEWER_ORIGIN}; img-src 'none'; font-src 'none'; base-uri 'none'; \
         form-action 'none'; frame-src 'none'; object-src 'none'"
    )
}

/// The byte at `index` of every test PDF body; `PDF_BYTE_JS` computes the same.
fn pdf_byte(index: usize) -> u8 {
    (index.wrapping_mul(7).wrapping_add(index >> 8) & 0xff) as u8
}

const PDF_BYTE_JS: &str = "const pdfByte = index => (index * 7 + (index >> 8)) & 0xff;";

fn pdf_body(length: usize) -> Vec<u8> {
    (0..length).map(pdf_byte).collect()
}

/// Helpers that every test viewer module starts with.
const VIEWER_PRELUDE: &str = "
const root = document.documentElement;
const report = text => { document.title = `report:${location.pathname}:${text}`; };
const settled = () => new Promise(resolve => {
    const check = () => root.dataset.pdfState !== 'loading';
    if (check()) {
        resolve(root.dataset.pdfState);
        return;
    }
    new MutationObserver((records, observer) => {
        if (check()) {
            observer.disconnect();
            resolve(root.dataset.pdfState);
        }
    }).observe(root, { attributes: true });
});
";

/// A viewer module that sends `worker.mjs` the PDF's blob URL and its own URL once the body
/// is in, and reports what the worker answers.
const WORKER_VIEWER_MODULE: &str = "
const startWorker = () => {
    const worker = new Worker('combray-pdf://viewer-test/worker.mjs', { type: 'module' });
    worker.onmessage = ({ data }) => report(data);
    worker.onerror = event => report(`worker error: ${event.message}`);
    return worker;
};
settled().then(state => {
    if (state !== 'ready') {
        report(`state ${state}`);
        return;
    }
    startWorker().postMessage({ src: root.dataset.pdfSrc, page: location.href });
});
";

/// A worker that fetches the blob URL it is sent and compares the bytes with the pattern.
const COMPARING_WORKER: &str = "
onmessage = async ({ data: { src } }) => {
    try {
        const bytes = new Uint8Array(await (await fetch(src)).arrayBuffer());
        const mismatch = bytes.findIndex((byte, index) => byte !== pdfByte(index));
        postMessage(mismatch === -1 ? `${bytes.length} bytes match` : `byte ${mismatch} differs`);
    } catch (error) {
        postMessage(`${error.name}: ${error.message}`);
    }
};
";

/// The files the test `combray-pdf:` scheme serves, by path: content type, body and policy.
struct ViewerFiles(HashMap<String, (&'static str, Vec<u8>, String)>);

impl ViewerFiles {
    fn new(module: &str) -> Self {
        let source = format!("{PDF_BYTE_JS}\n{VIEWER_PRELUDE}\n{module}");
        Self(HashMap::new()).file("/viewer.mjs", "text/javascript", source.into_bytes())
    }

    fn worker(self, worker: &str) -> Self {
        self.worker_with_csp("/worker.mjs", worker, viewer_csp())
    }

    /// A worker at `path` whose response carries `csp`.
    fn worker_with_csp(self, path: &str, worker: &str, csp: String) -> Self {
        let source = format!("{PDF_BYTE_JS}\n{worker}");
        self.file_with_csp(path, "text/javascript", source.into_bytes(), csp)
    }

    fn file(self, path: &str, content_type: &'static str, body: Vec<u8>) -> Self {
        self.file_with_csp(path, content_type, body, viewer_csp())
    }

    fn file_with_csp(
        mut self,
        path: &str,
        content_type: &'static str,
        body: Vec<u8>,
        csp: String,
    ) -> Self {
        self.0.insert(path.to_owned(), (content_type, body, csp));
        self
    }
}

/// Stands in for the embedder's `combray-pdf:` scheme: GET only, on the viewer's host, for
/// a fixed set of files, and every response carries a policy, the viewer's unless a test
/// gives a file another.
struct TestViewerProtocol {
    files: ViewerFiles,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ProtocolHandler for TestViewerProtocol {
    fn load<'a>(
        &'a self,
        request: &'a mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send + 'a>> {
        let url = request.current_url();
        self.requests.lock().unwrap().push(url.path().to_owned());
        let file = (request.method == Method::GET && url.host_str() == Some(VIEWER_HOST))
            .then(|| self.files.0.get(url.path()))
            .flatten();
        let Some((content_type, body, csp)) = file else {
            return Box::pin(ready(Response::network_error(
                NetworkError::ResourceLoadError("Not a viewer file".to_owned()),
            )));
        };

        let mut response = Response::new(url, ResourceFetchTiming::new(request.timing_type()));
        response
            .headers
            .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
        response
            .headers
            .insert(CONTENT_SECURITY_POLICY, HeaderValue::from_str(csp).unwrap());
        *response.body.lock() = ResponseBody::Done(body.clone());
        Box::pin(ready(response))
    }

    fn is_fetchable(&self) -> bool {
        true
    }
}

#[derive(Default)]
struct TestDelegate {
    rendering_context: RefCell<Option<Rc<dyn RenderingContext>>>,
    load_complete: Cell<bool>,
    popup: RefCell<Option<WebView>>,
}

impl WebViewDelegate for TestDelegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        webview.paint();
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        if status == LoadStatus::Complete {
            self.load_complete.set(true);
        }
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        let Some(rendering_context) = self.rendering_context.borrow().clone() else {
            return;
        };
        let popup = request
            .builder(rendering_context)
            .delegate(parent_webview.delegate())
            .build();
        self.popup.borrow_mut().replace(popup);
    }
}

/// Preferences with the viewer on and no proxy (the test servers are on localhost).
fn viewer_preferences() -> Preferences {
    let mut preferences = Preferences::default();
    preferences.network_http_proxy_uri = String::new();
    preferences.network_https_proxy_uri = String::new();
    preferences.combray_pdf_viewer_module = format!("{VIEWER_ORIGIN}viewer.mjs");
    preferences.combray_pdf_viewer_csp = viewer_csp();
    preferences
}

struct PdfTest {
    servo_test: ServoTest,
    delegate: Rc<TestDelegate>,
    webview: WebView,
    viewer_requests: Arc<Mutex<Vec<String>>>,
}

impl PdfTest {
    fn new(files: ViewerFiles, customize: impl FnOnce(&mut Preferences)) -> Self {
        let mut preferences = viewer_preferences();
        customize(&mut preferences);

        let viewer_requests = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ProtocolRegistry::default();
        registry
            .register(
                "combray-pdf",
                TestViewerProtocol {
                    files,
                    requests: viewer_requests.clone(),
                },
            )
            .expect("Should be able to register the viewer scheme");
        let servo_test = ServoTest::new_with_builder(move |builder| {
            builder.preferences(preferences).protocol_registry(registry)
        });

        let delegate = Rc::new(TestDelegate::default());
        delegate
            .rendering_context
            .replace(Some(servo_test.rendering_context.clone()));
        let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
            .delegate(delegate.clone())
            .build();
        let test = Self {
            servo_test,
            delegate,
            webview,
            viewer_requests,
        };
        // A load before the initial `about:blank` is in place would be lost, and its late
        // completion would pass for that of the next load.
        test.spin_until("the initial document", || {
            test.webview.url().is_some() && test.delegate.load_complete.get()
        });
        test
    }

    fn spin_until(&self, what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for {what} (at {:?}, {:?}, title {:?})",
                self.webview.url(),
                self.webview.load_status(),
                self.webview.page_title()
            );
            self.servo_test.servo().spin_event_loop();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Spin the event loop for `duration`, for things that must not happen.
    fn spin_for(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        self.spin_until("nothing", || Instant::now() >= deadline);
    }

    /// Navigate to `url` and wait until that document has loaded.
    fn load(&self, url: &Url) {
        self.delegate.load_complete.set(false);
        self.webview.load(url.clone());
        self.spin_until(&format!("the load of {url}"), || {
            self.delegate.load_complete.get() && self.webview.url().as_ref() == Some(url)
        });
    }

    fn evaluate_in(&self, webview: &WebView, script: &str) -> String {
        match evaluate_javascript(&self.servo_test, webview.clone(), script) {
            Ok(JSValue::String(value)) => value,
            other => panic!("Expected a string from {script:?}, got {other:?}"),
        }
    }

    fn evaluate(&self, script: &str) -> String {
        self.evaluate_in(&self.webview, script)
    }

    /// Wait for a viewer module to report for the document at `path`, and return the report.
    fn wait_for_report(&self, webview: &WebView, path: &str) -> String {
        let prefix = format!("report:{path}:");
        self.spin_until(&format!("a report from {path}"), || {
            webview
                .page_title()
                .is_some_and(|title| title.starts_with(&prefix))
        });
        webview.page_title().unwrap()[prefix.len()..].to_owned()
    }

    fn viewer_requests(&self) -> Vec<String> {
        self.viewer_requests.lock().unwrap().clone()
    }

    /// The bytes that blob URL store entries hold in memory.
    fn blob_store_bytes(&self) -> usize {
        let report: Arc<Mutex<Option<MemoryReportResult>>> = Arc::default();
        let report_from_callback = report.clone();
        let callback = GenericCallback::new(move |result| {
            if let Ok(result) = result {
                *report_from_callback.lock().unwrap() = Some(result);
            }
        })
        .expect("Should be able to create a memory report callback");
        self.servo_test.servo().create_memory_report(callback);
        self.spin_until("a memory report", || report.lock().unwrap().is_some());

        let result = report.lock().unwrap().take().unwrap();
        result
            .results
            .iter()
            .flat_map(|process| process.reports.iter())
            .filter(|report| report.path == ["blob-url-store", "memory"])
            .map(|report| report.size)
            .sum()
    }
}

type HttpResponse = HyperResponse<BoxBody<Bytes, hyper::Error>>;

/// The paths of the requests the test server received.
#[derive(Clone, Default)]
struct ServerLog(Arc<Mutex<Vec<String>>>);

impl ServerLog {
    fn count(&self, path: &str) -> usize {
        self.0.lock().unwrap().iter().filter(|p| *p == path).count()
    }
}

/// An HTTP server on localhost that answers each request with `route(path, response)`.
fn start_server(
    route: impl Fn(&str, &mut HttpResponse) + Send + Sync + 'static,
) -> (Server, Url, ServerLog) {
    let log = ServerLog::default();
    let server_log = log.clone();
    let (server, url) = make_server(
        move |request: HyperRequest<Incoming>, response: &mut HttpResponse| {
            let path = request.uri().path().to_owned();
            server_log.0.lock().unwrap().push(path.clone());
            route(&path, response);
        },
    );
    (server, url.as_url().clone(), log)
}

/// `base` on a host that, unlike `localhost`, is not potentially trustworthy: pages from it
/// are no secure contexts, so mixed content checks do not block the viewer's scheme there.
fn untrustworthy_base(base: &Url) -> Url {
    const HOST: &str = "pages.test";
    replace_host_table(HashMap::from([(
        HOST.to_owned(),
        "127.0.0.1".parse().unwrap(),
    )]));
    let mut untrustworthy_base = base.clone();
    untrustworthy_base.set_host(Some(HOST)).unwrap();
    untrustworthy_base
}

fn set_header(response: &mut HttpResponse, name: &'static str, value: &str) {
    response.headers_mut().append(
        HeaderName::from_static(name),
        HeaderValue::from_str(value).unwrap(),
    );
}

/// Serve a PDF of `length` bytes with a `Content-Length`.
fn serve_pdf(response: &mut HttpResponse, length: usize) {
    set_header(response, "content-type", "application/pdf");
    *response.body_mut() = make_body(pdf_body(length));
}

fn serve_html(response: &mut HttpResponse, html: &str) {
    set_header(response, "content-type", "text/html");
    *response.body_mut() = make_body(html.as_bytes().to_vec());
}

/// What the server did with a streamed body, and whether the test holds it back.
#[derive(Default)]
struct BodyProbe {
    produced: AtomicUsize,
    dropped: AtomicBool,
    /// The body stops after this many bytes, or before it ends, until `release`.
    hold_at: Option<usize>,
    released: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl BodyProbe {
    fn holding_at(hold_at: usize) -> Self {
        Self {
            hold_at: Some(hold_at),
            ..Self::default()
        }
    }

    /// Let a held body continue.
    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }

    /// Whether the body must wait at `position`. The waker is stored before the second
    /// check, so a release in between is not lost.
    fn holds(&self, position: usize, context: &Context<'_>) -> bool {
        if self.hold_at.is_none_or(|hold_at| position < hold_at) {
            return false;
        }
        if self.released.load(Ordering::SeqCst) {
            return false;
        }
        *self.waker.lock().unwrap() = Some(context.waker().clone());
        !self.released.load(Ordering::SeqCst)
    }
}

/// A PDF body produced as the client reads it, in 64 KiB chunks. The server drops it when
/// it was sent in full or when the client closed the connection.
struct PatternBody {
    length: usize,
    probe: Arc<BodyProbe>,
}

impl Body for PatternBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let start = self.probe.produced.load(Ordering::SeqCst);
        if self.probe.holds(start, context) {
            return Poll::Pending;
        }
        if start >= self.length {
            return Poll::Ready(None);
        }
        let end = (start + 64 * 1024).min(self.length);
        self.probe.produced.store(end, Ordering::SeqCst);
        let chunk: Vec<u8> = (start..end).map(pdf_byte).collect();
        Poll::Ready(Some(Ok(Frame::data(Bytes::from(chunk)))))
    }
}

impl Drop for PatternBody {
    fn drop(&mut self) {
        self.probe.dropped.store(true, Ordering::SeqCst);
    }
}

/// Serve a streamed PDF of `length` bytes, chunked unless `content_length` is set.
fn serve_streamed_pdf(
    response: &mut HttpResponse,
    length: usize,
    content_length: bool,
    probe: &Arc<BodyProbe>,
) {
    set_header(response, "content-type", "application/pdf");
    if content_length {
        set_header(response, "content-length", &length.to_string());
    }
    *response.body_mut() = PatternBody {
        length,
        probe: probe.clone(),
    }
    .boxed();
}

/// A viewer module that reports once the root element's state has settled.
const SETTLED_VIEWER_MODULE: &str = "settled().then(state => report(state));";

const STATE_SCRIPT: &str = "JSON.stringify([
    document.documentElement.dataset.pdfState,
    document.documentElement.dataset.pdfLength,
    document.documentElement.dataset.pdfLimit,
    document.documentElement.dataset.pdfSrc,
])";

pdf_viewer_test! {
    /// Unless both string preferences are set, a PDF keeps the "Unknown content type" page.
    fn test_pdf_viewer_off_keeps_stand_in() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|_, response| serve_pdf(response, PDF_LENGTH));

        let configurations = [
            ("off", String::new(), String::new()),
            ("module-only", format!("{VIEWER_ORIGIN}viewer.mjs"), String::new()),
            ("csp-only", String::new(), viewer_csp()),
        ];
        for (name, module, csp) in configurations {
            let mut preferences = prefs::get().clone();
            preferences.combray_pdf_viewer_module = module;
            preferences.combray_pdf_viewer_csp = csp;
            prefs::set(preferences);

            test.load(&base.join(&format!("/doc.pdf?{name}")).unwrap());
            let page = test.evaluate(
                "JSON.stringify([document.documentElement.hasAttribute('data-pdf-state'), \
                 document.body.textContent])",
            );
            assert_eq!(
                page, r#"[false,"Unknown content type (application/pdf)."]"#,
                "{name}"
            );
        }
        assert_eq!(log.count("/doc.pdf"), 3);
        assert_eq!(test.viewer_requests(), Vec::<String>::new());
        server.close();
    }
}

pdf_viewer_test! {
    /// The viewer document keeps the PDF's URL and content type, gets an opaque origin, and
    /// loads the viewer module; MIME type parameters do not matter. Whether it is a secure
    /// context follows the URL, and `localhost` is potentially trustworthy.
    fn test_pdf_viewer_document_identity() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|path, response| {
            serve_pdf(response, PDF_LENGTH);
            if path == "/params.pdf" {
                response.headers_mut().remove("content-type");
                set_header(response, "content-type", "application/pdf; x=y");
            }
        });

        for (path, content_type) in [
            ("/doc.pdf", "application/pdf"),
            ("/params.pdf", "application/pdf;x=y"),
        ] {
            let url = base.join(path).unwrap();
            test.load(&url);
            assert_eq!(test.wait_for_report(&test.webview, path), "ready");
            let identity = test.evaluate(
                "JSON.stringify([
                    window.origin,
                    isSecureContext,
                    document.contentType.replace(/ /g, ''),
                    document.URL,
                    document.compatMode,
                    document.querySelector('script[type=module]').src,
                    document.documentElement.dataset.pdfState,
                    document.documentElement.dataset.pdfLength,
                    document.documentElement.dataset.pdfSrc.replace(/[0-9a-f-]{36}$/, 'ID'),
                ])",
            );
            assert_eq!(
                identity,
                format!(
                    r#"["null",true,"{content_type}","{url}","CSS1Compat","{}","ready","{}","{}"]"#,
                    format!("{VIEWER_ORIGIN}viewer.mjs"),
                    PDF_LENGTH,
                    "blob:null/ID",
                ),
                "{path}"
            );
            assert_eq!(log.count(path), 1, "{path}");
        }
        server.close();
    }
}

pdf_viewer_test! {
    /// A module worker fetches the body from the blob URL, byte for byte, and the PDF is
    /// requested only once.
    fn test_pdf_viewer_worker_reads_body_from_blob() {
        const CHUNKED_LENGTH: usize = 1024 * 1024 + 13;
        let probe = Arc::new(BodyProbe::default());
        let server_probe = probe.clone();
        let test = PdfTest::new(
            ViewerFiles::new(WORKER_VIEWER_MODULE).worker(COMPARING_WORKER),
            |_| {},
        );
        let (server, base, log) = start_server(move |path, response| match path {
            "/chunked.pdf" => serve_streamed_pdf(response, CHUNKED_LENGTH, false, &server_probe),
            _ => serve_pdf(response, PDF_LENGTH),
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(
            test.wait_for_report(&test.webview, "/doc.pdf"),
            format!("{PDF_LENGTH} bytes match")
        );
        assert_eq!(log.count("/doc.pdf"), 1);

        // Without a Content-Length, the length is unknown until the end.
        test.load(&base.join("/chunked.pdf").unwrap());
        assert_eq!(
            test.wait_for_report(&test.webview, "/chunked.pdf"),
            format!("{CHUNKED_LENGTH} bytes match")
        );
        assert_eq!(test.evaluate("document.documentElement.dataset.pdfLength"), "");
        assert_eq!(log.count("/chunked.pdf"), 1);
        server.close();
    }
}

pdf_viewer_test! {
    /// The viewer's policy applies, and the policies of the PDF response, enforced or
    /// report-only, do not.
    fn test_pdf_viewer_enforces_viewer_csp() {
        let module = "
            let evaluation = 'eval ran';
            try {
                eval('1');
            } catch (error) {
                evaluation = error.name;
            }
            const script = document.createElement('script');
            script.textContent = 'window.inlineRan = true;';
            document.head.append(script);
            report(`${evaluation}:${window.inlineRan ? 'inline ran' : 'inline blocked'}`);
        ";
        let test = PdfTest::new(ViewerFiles::new(module), |_| {});
        let (server, base, log) = start_server(|path, response| {
            if path == "/csp-report" {
                return;
            }
            serve_pdf(response, PDF_LENGTH);
            // This would block the viewer module if it applied.
            set_header(response, "content-security-policy", "script-src 'none'");
            // This would send the PDF's site reports that name the viewer's URLs.
            set_header(
                response,
                "content-security-policy-report-only",
                "script-src 'none'; report-uri /csp-report",
            );
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(
            test.wait_for_report(&test.webview, "/doc.pdf"),
            "EvalError:inline blocked"
        );
        test.spin_for(Duration::from_millis(500));
        assert_eq!(log.count("/csp-report"), 0);
        server.close();
    }
}

pdf_viewer_test! {
    /// A `sandbox` directive in the PDF response's policy does not stop the viewer module.
    fn test_pdf_viewer_ignores_csp_sandbox() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(|_, response| {
            serve_pdf(response, PDF_LENGTH);
            set_header(response, "content-security-policy", "sandbox");
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        server.close();
    }
}

/// Load a PDF larger than the viewer accepts and check that the viewer refuses it and that
/// the server stopped sending it early. With a `Content-Length`, the body is held back until
/// the refusal, which must come from the headers alone.
fn check_too_large_pdf(content_length: bool) {
    let probe = Arc::new(if content_length {
        BodyProbe::holding_at(0)
    } else {
        BodyProbe::default()
    });
    let server_probe = probe.clone();
    let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |preferences| {
        preferences.combray_pdf_viewer_max_bytes = SMALL_LIMIT;
    });
    let (server, base, log) = start_server(move |_, response| {
        serve_streamed_pdf(response, LARGE_PDF_LENGTH, content_length, &server_probe)
    });

    test.load(&base.join("/large.pdf").unwrap());
    assert_eq!(
        test.wait_for_report(&test.webview, "/large.pdf"),
        "too-large"
    );
    let length = if content_length {
        LARGE_PDF_LENGTH.to_string()
    } else {
        String::new()
    };
    assert_eq!(
        test.evaluate(STATE_SCRIPT),
        format!(r#"["too-large","{length}","{SMALL_LIMIT}",null]"#)
    );
    if content_length {
        assert_eq!(probe.produced.load(Ordering::SeqCst), 0);
        // The network layer notices the cancellation at the next chunk.
        probe.release();
    }

    // The navigation fetch was cancelled, so the connection closed long before the end.
    test.spin_until("the server to drop the body", || {
        probe.dropped.load(Ordering::SeqCst)
    });
    let produced = probe.produced.load(Ordering::SeqCst);
    println!("The server produced {produced} of {LARGE_PDF_LENGTH} bytes");
    assert!(
        produced < LARGE_PDF_LENGTH / 2,
        "The server produced {produced} of {LARGE_PDF_LENGTH} bytes"
    );
    assert_eq!(log.count("/large.pdf"), 1);
    assert_eq!(test.blob_store_bytes(), 0);
    server.close();
}

pdf_viewer_test! {
    /// A `Content-Length` above the limit is refused as soon as the headers arrive.
    fn test_pdf_viewer_too_large_at_headers() {
        check_too_large_pdf(true);
    }
}

pdf_viewer_test! {
    /// A chunked body is refused once it grows past the limit.
    fn test_pdf_viewer_too_large_mid_stream() {
        check_too_large_pdf(false);
    }
}

pdf_viewer_test! {
    /// A body exactly as large as the limit is accepted, with or without `Content-Length`.
    fn test_pdf_viewer_accepts_body_at_limit() {
        let chunked_probe = Arc::new(BodyProbe::default());
        let fixed_probe = Arc::new(BodyProbe::default());
        let server_probes = (chunked_probe.clone(), fixed_probe.clone());
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |preferences| {
            preferences.combray_pdf_viewer_max_bytes = PDF_LENGTH as u64;
        });
        let (server, base, _) = start_server(move |path, response| match path {
            "/chunked.pdf" => serve_streamed_pdf(response, PDF_LENGTH, false, &server_probes.0),
            _ => serve_streamed_pdf(response, PDF_LENGTH, true, &server_probes.1),
        });

        for (path, length) in [
            ("/chunked.pdf", String::new()),
            ("/fixed.pdf", PDF_LENGTH.to_string()),
        ] {
            test.load(&base.join(path).unwrap());
            assert_eq!(test.wait_for_report(&test.webview, path), "ready", "{path}");
            let state = test.evaluate(
                "JSON.stringify([
                    document.documentElement.dataset.pdfState,
                    document.documentElement.dataset.pdfLength,
                    document.documentElement.dataset.pdfLimit,
                    document.documentElement.dataset.pdfSrc.startsWith('blob:null/'),
                ])",
            );
            assert_eq!(state, format!(r#"["ready","{length}",null,true]"#), "{path}");
        }
        assert_eq!(chunked_probe.produced.load(Ordering::SeqCst), PDF_LENGTH);
        assert_eq!(fixed_probe.produced.load(Ordering::SeqCst), PDF_LENGTH);
        server.close();
    }
}

pdf_viewer_test! {
    /// Leaving a viewer while its body is still arriving aborts its parser. The viewer then
    /// drops the body and cancels the navigation fetch, which would otherwise keep
    /// downloading the rest with nobody reading it.
    fn test_pdf_viewer_aborted_load_cancels_fetch() {
        const HOLD_AT: usize = 1024 * 1024;
        let probe = Arc::new(BodyProbe::holding_at(HOLD_AT));
        let server_probe = probe.clone();
        // The module reports at once, while the body is held back.
        let test = PdfTest::new(ViewerFiles::new("report(root.dataset.pdfState);"), |_| {});
        let (server, base, log) = start_server(move |path, response| match path {
            "/other.html" => serve_html(response, "<!DOCTYPE html>other"),
            _ => serve_streamed_pdf(response, LARGE_PDF_LENGTH, false, &server_probe),
        });

        test.webview.load(base.join("/large.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/large.pdf"), "loading");
        test.spin_until("the server to hold the body", || {
            probe.produced.load(Ordering::SeqCst) >= HOLD_AT
        });

        // A navigation aborts the document it leaves. The test helper would wait for the
        // viewer to finish loading, which it cannot while the body is held back.
        let other_url = base.join("/other.html").unwrap();
        test.delegate.load_complete.set(false);
        test.webview
            .evaluate_javascript("location.href = '/other.html'", |_| {});
        test.spin_until("the other page", || {
            test.delegate.load_complete.get() && test.webview.url().as_ref() == Some(&other_url)
        });

        // The script thread can only cancel when the next chunk arrives.
        probe.release();
        test.spin_until("the server to drop the body", || {
            probe.dropped.load(Ordering::SeqCst)
        });
        let produced = probe.produced.load(Ordering::SeqCst);
        println!("The server produced {produced} of {LARGE_PDF_LENGTH} bytes");
        assert!(
            produced < LARGE_PDF_LENGTH / 2,
            "The server produced {produced} of {LARGE_PDF_LENGTH} bytes"
        );
        assert_eq!(log.count("/large.pdf"), 1);
        server.close();
    }
}

pdf_viewer_test! {
    /// A PDF in an iframe keeps the "Unknown content type" page; the viewer is top level only.
    fn test_pdf_in_iframe_keeps_stand_in() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|path, response| match path {
            "/frame.html" => serve_html(response, "<!DOCTYPE html><iframe src=/doc.pdf></iframe>"),
            _ => serve_pdf(response, PDF_LENGTH),
        });

        // A viewer in a frame of a secure context could not load its module anyway.
        test.load(&untrustworthy_base(&base).join("/frame.html").unwrap());
        test.spin_for(Duration::from_millis(500));
        assert_eq!(log.count("/doc.pdf"), 1);
        // The frame's document is opaque, but a viewer there would load the viewer module.
        assert_eq!(test.viewer_requests(), Vec::<String>::new());
        server.close();
    }
}

pdf_viewer_test! {
    /// A module worker served by the viewer scheme fetches a file from the scheme and
    /// compiles WebAssembly under `'wasm-unsafe-eval'`.
    fn test_pdf_viewer_scheme_worker_compiles_wasm() {
        // A module that exports `answer`, which returns 42.
        const ANSWER_WASM: &[u8] = &[
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x05, 0x01, 0x60, 0x00, 0x01,
            0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x0a, 0x01, 0x06, 0x61, 0x6e, 0x73, 0x77, 0x65,
            0x72, 0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x2a, 0x0b,
        ];
        let worker = "
            onmessage = async () => {
                try {
                    const response = await fetch('combray-pdf://viewer-test/answer.wasm');
                    const bytes = await response.arrayBuffer();
                    const { instance } = await WebAssembly.instantiate(bytes);
                    postMessage(`answer ${instance.exports.answer()}`);
                } catch (error) {
                    postMessage(`${error.name}: ${error.message}`);
                }
            };
        ";
        let files = ViewerFiles::new(WORKER_VIEWER_MODULE)
            .worker(worker)
            .file("/answer.wasm", "application/wasm", ANSWER_WASM.to_vec());
        let test = PdfTest::new(files, |_| {});
        let (server, base, _) = start_server(|_, response| serve_pdf(response, PDF_LENGTH));

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "answer 42");
        assert_eq!(
            test.viewer_requests(),
            ["/viewer.mjs", "/worker.mjs", "/answer.wasm"]
        );
        server.close();
    }
}

pdf_viewer_test! {
    /// A worker runs under the policy of its own scheme response, not the document's: with
    /// the viewer's policy, no `eval` and no network; with `'unsafe-eval'` added, `eval`.
    fn test_pdf_viewer_worker_blocks_eval_and_network() {
        let module = "
            const run = path => new Promise(resolve => {
                const worker = new Worker(`combray-pdf://viewer-test${path}`, { type: 'module' });
                worker.onmessage = ({ data }) => resolve(data);
                worker.onerror = event => resolve(`worker error: ${event.message}`);
                worker.postMessage({ page: location.href });
            });
            Promise.all([run('/worker.mjs'), run('/eval-worker.mjs')])
                .then(results => report(results.join(' ')));
        ";
        // The worker tries to reach the server that sent the PDF.
        let worker = "
            onmessage = async ({ data: { page } }) => {
                let evaluation = 'eval ran';
                try {
                    eval('1');
                } catch (error) {
                    evaluation = error.name;
                }
                let request = 'fetch ran';
                try {
                    await fetch(new URL('/probe', page));
                } catch (error) {
                    request = error.name;
                }
                postMessage(`${evaluation}:${request}`);
            };
        ";
        let eval_csp = viewer_csp().replace(
            "script-src combray-pdf://viewer-test/",
            "script-src combray-pdf://viewer-test/ 'unsafe-eval'",
        );
        assert_ne!(eval_csp, viewer_csp());
        let files = ViewerFiles::new(module).worker(worker).worker_with_csp(
            "/eval-worker.mjs",
            worker,
            eval_csp,
        );
        let test = PdfTest::new(files, |_| {});
        let (server, base, log) = start_server(|_, response| serve_pdf(response, PDF_LENGTH));

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(
            test.wait_for_report(&test.webview, "/doc.pdf"),
            "EvalError:TypeError eval ran:TypeError"
        );
        assert_eq!(log.count("/probe"), 0);
        server.close();
    }
}

pdf_viewer_test! {
    /// Back to a viewer in the back-forward cache, the worker reads the blob again without
    /// a new request.
    fn test_pdf_viewer_bfcache_refetches_blob() {
        let module = format!(
            "{WORKER_VIEWER_MODULE}
            addEventListener('pageshow', event => {{
                if (event.persisted) {{
                    const worker = startWorker();
                    worker.onmessage = ({{ data }}) => {{ document.title = `again:${{data}}`; }};
                    worker.postMessage({{ src: root.dataset.pdfSrc }});
                }}
            }});"
        );
        let test = PdfTest::new(
            ViewerFiles::new(&module).worker(COMPARING_WORKER),
            |_| {},
        );
        let (server, base, log) = start_server(|path, response| match path {
            "/other.html" => serve_html(response, "<!DOCTYPE html>other"),
            _ => serve_pdf(response, PDF_LENGTH),
        });

        let pdf_url = base.join("/doc.pdf").unwrap();
        test.load(&pdf_url);
        let matched = format!("{PDF_LENGTH} bytes match");
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), matched);

        test.load(&base.join("/other.html").unwrap());
        let _ = test.webview.go_back(1);
        test.spin_until("the viewer to read the blob again", || {
            test.webview
                .page_title()
                .is_some_and(|title| title.starts_with("again:"))
        });
        assert_eq!(test.webview.page_title().unwrap(), format!("again:{matched}"));
        assert_eq!(test.webview.url(), Some(pdf_url));
        assert_eq!(log.count("/doc.pdf"), 1);
        server.close();
    }
}

pdf_viewer_test! {
    /// Another document cannot read the viewer's blob URL.
    fn test_pdf_viewer_blob_not_readable_elsewhere() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(|path, response| match path {
            "/other.html" => serve_html(response, "<!DOCTYPE html>other"),
            _ => serve_pdf(response, PDF_LENGTH),
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        let src = test.evaluate("document.documentElement.dataset.pdfSrc");
        assert!(test.blob_store_bytes() >= PDF_LENGTH);

        // The viewer stays in the back-forward cache, so its blob stays valid.
        test.load(&base.join("/other.html").unwrap());
        assert!(test.blob_store_bytes() >= PDF_LENGTH);
        test.evaluate(&format!(
            "fetch('{src}').then(
                response => response.arrayBuffer(),
            ).then(
                body => {{ document.title = `fetched ${{body.byteLength}}`; }},
                error => {{ document.title = `refused ${{error.name}}`; }},
            ); ''"
        ));
        test.spin_until("the fetch to settle", || {
            test.webview
                .page_title()
                .is_some_and(|title| title.starts_with("fetched") || title.starts_with("refused"))
        });
        assert_eq!(test.webview.page_title().unwrap(), "refused TypeError");
        server.close();
    }
}

pdf_viewer_test! {
    /// The test scheme, like the embedder's, is not secure, so a secure context cannot fetch
    /// the viewer's files as mixed content.
    fn test_pdf_viewer_scheme_is_mixed_content_for_secure_pages() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(|_, response| serve_html(response, "page"));

        // Each page reports with its host, so the title of the previous page cannot pass for
        // its report.
        let fetch_viewer_module = "
            fetch('combray-pdf://viewer-test/viewer.mjs').then(
                response => `fetched ${response.status}`,
                error => `refused ${error.name}`,
            ).then(
                outcome => { document.title = `${location.hostname} ${outcome}`; },
            ); ''";
        let fetch_outcome = |page: &Url| {
            test.load(page);
            test.evaluate(fetch_viewer_module);
            let prefix = format!("{} ", page.host_str().unwrap());
            test.spin_until("the fetch to settle", || {
                test.webview
                    .page_title()
                    .is_some_and(|title| title.starts_with(&prefix))
            });
            test.webview.page_title().unwrap()[prefix.len()..].to_owned()
        };

        // The scheme is fetchable, so other pages can read it; the embedder's random host
        // is what keeps them from finding it.
        let insecure_page = untrustworthy_base(&base).join("/page.html").unwrap();
        assert_eq!(fetch_outcome(&insecure_page), "fetched 200");
        assert_eq!(test.viewer_requests(), ["/viewer.mjs"]);
        // `localhost` is potentially trustworthy.
        assert_eq!(
            fetch_outcome(&base.join("/page.html").unwrap()),
            "refused TypeError"
        );
        assert_eq!(test.viewer_requests(), ["/viewer.mjs"]);
        server.close();
    }
}

pdf_viewer_test! {
    /// Discarding a viewer from session history frees its body.
    fn test_pdf_viewer_blob_released_on_eviction() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |preferences| {
            preferences.session_history_max_length = 1;
        });
        let (server, base, _) = start_server(|path, response| match path {
            "/a.html" | "/b.html" => serve_html(response, "<!DOCTYPE html>page"),
            _ => serve_pdf(response, PDF_LENGTH),
        });
        let baseline = test.blob_store_bytes();

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        assert!(test.blob_store_bytes() >= baseline + PDF_LENGTH);

        // One step back, the viewer stays in the back-forward cache with its body.
        test.load(&base.join("/a.html").unwrap());
        assert!(test.blob_store_bytes() >= baseline + PDF_LENGTH);

        // Two steps back, it is discarded.
        test.load(&base.join("/b.html").unwrap());
        test.spin_until("the blob to be released", || {
            test.blob_store_bytes() == baseline
        });
        server.close();
    }
}

pdf_viewer_test! {
    /// A viewer that cannot enter the back-forward cache frees its body when it unloads.
    fn test_pdf_viewer_blob_released_on_unsalvageable_unload() {
        // An `unload` listener makes a document unsalvageable.
        let module = "addEventListener('unload', () => {}); settled().then(report);";
        let test = PdfTest::new(ViewerFiles::new(module), |_| {});
        let (server, base, _) = start_server(|path, response| match path {
            "/other.html" => serve_html(response, "<!DOCTYPE html>other"),
            _ => serve_pdf(response, PDF_LENGTH),
        });
        let baseline = test.blob_store_bytes();

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        assert!(test.blob_store_bytes() >= baseline + PDF_LENGTH);

        test.load(&base.join("/other.html").unwrap());
        test.spin_until("the blob to be released", || {
            test.blob_store_bytes() == baseline
        });
        server.close();
    }
}

pdf_viewer_test! {
    /// A body without a `Content-Length` grows as it arrives, but the blob store keeps it
    /// without the spare capacity.
    fn test_pdf_viewer_blob_has_no_spare_capacity() {
        const CHUNKED_LENGTH: usize = 1024 * 1024 + 13;
        let probe = Arc::new(BodyProbe::default());
        let server_probe = probe.clone();
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(move |_, response| {
            serve_streamed_pdf(response, CHUNKED_LENGTH, false, &server_probe)
        });
        let baseline = test.blob_store_bytes();

        test.load(&base.join("/chunked.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/chunked.pdf"), "ready");
        // The allocator rounds a fitted buffer up a little; a grown one can be twice as large.
        let held = test.blob_store_bytes() - baseline;
        println!("The blob store holds {held} bytes for a body of {CHUNKED_LENGTH}");
        assert!(
            (CHUNKED_LENGTH..CHUNKED_LENGTH * 3 / 2).contains(&held),
            "The blob store holds {held} bytes for a body of {CHUNKED_LENGTH}"
        );
        server.close();
    }
}

pdf_viewer_test! {
    /// `Link` headers of the PDF response do not act in the viewer document.
    fn test_pdf_viewer_ignores_link_headers() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|path, response| match path {
            "/page.html" => {
                serve_html(response, "<!DOCTYPE html>page");
                set_header(response, "link", "</page-preload.css>; rel=preload; as=style");
            },
            "/doc.pdf" => {
                serve_pdf(response, PDF_LENGTH);
                set_header(response, "link", "</pdf-preload.css>; rel=preload; as=style");
            },
            _ => {
                set_header(response, "content-type", "text/css");
                *response.body_mut() = make_body(b"html {}".to_vec());
            },
        });

        // The same header on a page does preload, so the check below means something.
        test.load(&base.join("/page.html").unwrap());
        test.spin_until("the page's preload", || log.count("/page-preload.css") == 1);

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        test.spin_for(Duration::from_millis(500));
        assert_eq!(log.count("/pdf-preload.css"), 0);
        server.close();
    }
}

pdf_viewer_test! {
    /// A popup opened from a document whose scripts are sandboxed could not run the viewer,
    /// so it keeps the "Unknown content type" page.
    fn test_pdf_viewer_scripts_sandboxed_opener_keeps_stand_in() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|path, response| match path {
            "/opener.html" => {
                serve_html(
                    response,
                    "<!DOCTYPE html><body style='margin: 0'>\
                     <a href=/doc.pdf target=_blank \
                     style='display: block; width: 100vw; height: 100vh'>PDF</a>",
                );
                set_header(response, "content-security-policy", "sandbox allow-popups");
            },
            _ => serve_pdf(response, PDF_LENGTH),
        });

        test.load(&base.join("/opener.html").unwrap());
        wait_for_webview_scene_to_be_up_to_date(&test.servo_test, &test.webview);
        click_at_point(&test.webview, DevicePoint::new(50., 50.), MouseButton::Primary);

        let pdf_url = base.join("/doc.pdf").unwrap();
        test.spin_until("the popup to load the PDF", || {
            test.delegate.popup.borrow().as_ref().is_some_and(|popup| {
                popup.url().as_ref() == Some(&pdf_url) &&
                    popup.load_status() == LoadStatus::Complete
            })
        });
        let popup = test.delegate.popup.borrow().clone().unwrap();
        // The URL can change while the load status is still that of the initial `about:blank`.
        test.spin_until("the popup's document to be parsed", || {
            test.evaluate_in(&popup, "document.readyState") == "complete"
        });
        let page = test.evaluate_in(
            &popup,
            "JSON.stringify([document.documentElement.hasAttribute('data-pdf-state'), \
             document.body.textContent])",
        );
        assert_eq!(page, r#"[false,"Unknown content type (application/pdf)."]"#);
        assert_eq!(log.count("/doc.pdf"), 1);
        assert_eq!(test.viewer_requests(), Vec::<String>::new());

        // Release the popup before Servo shuts down.
        *test.delegate.popup.borrow_mut() = None;
        server.close();
    }
}

pdf_viewer_test! {
    /// A popup that a secure page opens shows the viewer. The viewer is top level, so the
    /// opener's potentially trustworthy origin does not make its scheme mixed content.
    fn test_pdf_viewer_in_popup_of_secure_page() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, log) = start_server(|path, response| match path {
            "/opener.html" => serve_html(
                response,
                "<!DOCTYPE html><body style='margin: 0'>\
                 <div onclick=\"window.open('/doc.pdf')\" \
                 style='width: 100vw; height: 100vh'>PDF</div>",
            ),
            _ => serve_pdf(response, PDF_LENGTH),
        });

        // `localhost` is potentially trustworthy.
        test.load(&base.join("/opener.html").unwrap());
        wait_for_webview_scene_to_be_up_to_date(&test.servo_test, &test.webview);
        click_at_point(&test.webview, DevicePoint::new(50., 50.), MouseButton::Primary);

        let pdf_url = base.join("/doc.pdf").unwrap();
        test.spin_until("the popup to open the PDF", || {
            test.delegate
                .popup
                .borrow()
                .as_ref()
                .is_some_and(|popup| popup.url().as_ref() == Some(&pdf_url))
        });
        let popup = test.delegate.popup.borrow().clone().unwrap();
        assert_eq!(test.wait_for_report(&popup, "/doc.pdf"), "ready");
        assert_eq!(test.viewer_requests(), ["/viewer.mjs"]);
        assert_eq!(log.count("/doc.pdf"), 1);

        // Release the popup before Servo shuts down.
        drop(popup);
        *test.delegate.popup.borrow_mut() = None;
        server.close();
    }
}

pdf_viewer_test! {
    /// A body that fails to arrive leaves the viewer in the `failed` state, with no blob.
    fn test_pdf_viewer_reports_a_failed_body() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(|_, response| {
            serve_pdf(response, PDF_LENGTH);
            // The body is not gzip data, so decoding it fails.
            set_header(response, "content-encoding", "gzip");
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "failed");
        // With a content coding, `Content-Length` does not give the body's length.
        assert_eq!(test.evaluate(STATE_SCRIPT), r#"["failed","",null,null]"#);
        assert_eq!(test.blob_store_bytes(), 0);
        server.close();
    }
}

pdf_viewer_test! {
    /// A body that ends before its `Content-Length`, as when the connection drops, leaves the
    /// viewer in the `failed` state, with no blob.
    fn test_pdf_viewer_reports_a_truncated_body() {
        const SENT: usize = PDF_LENGTH / 2;
        let probe = Arc::new(BodyProbe::holding_at(SENT));
        let server_probe = probe.clone();
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(move |_, response| {
            serve_streamed_pdf(response, SENT, false, &server_probe);
            set_header(response, "content-length", &PDF_LENGTH.to_string());
        });

        test.webview.load(base.join("/doc.pdf").unwrap());
        // While the body is held back, the server sends what it has. A body that then ends
        // short of its `Content-Length` makes the server close the connection.
        test.spin_until("the server to hold the body", || {
            probe.produced.load(Ordering::SeqCst) >= SENT
        });
        test.spin_for(Duration::from_millis(500));
        probe.release();
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "failed");
        assert_eq!(
            test.evaluate(STATE_SCRIPT),
            format!(r#"["failed","{PDF_LENGTH}",null,null]"#)
        );
        assert_eq!(probe.produced.load(Ordering::SeqCst), SENT);
        assert_eq!(test.blob_store_bytes(), 0);
        server.close();
    }
}

pdf_viewer_test! {
    /// A PDF sent as an attachment opens in the viewer like any other top-level PDF (owner
    /// decision, 29 October 2026; Servo has no downloads).
    fn test_pdf_viewer_shows_attachment_pdf() {
        let test = PdfTest::new(ViewerFiles::new(SETTLED_VIEWER_MODULE), |_| {});
        let (server, base, _) = start_server(|_, response| {
            serve_pdf(response, PDF_LENGTH);
            set_header(response, "content-disposition", "attachment; filename=\"doc.pdf\"");
        });

        test.load(&base.join("/doc.pdf").unwrap());
        assert_eq!(test.wait_for_report(&test.webview, "/doc.pdf"), "ready");
        server.close();
    }
}
