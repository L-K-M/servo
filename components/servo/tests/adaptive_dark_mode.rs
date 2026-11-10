/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Adaptive dark mode paints light pages dark without changing their styles.
//!
//! The cases share one `Servo` because embedder tests in one process cannot each create one.

mod common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use image::RgbaImage;
use servo::{JSValue, LoadStatus, Theme, WebView, WebViewBuilder};
use url::Url;

use crate::common::{ServoTest, WebViewDelegateImpl, evaluate_javascript};

/// Screenshots to take, while waiting for a change to reach the screen.
const MAX_SCREENSHOTS: usize = 100;

const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// A page with an unstyled canvas, a big glyph of default text on the left half and a
/// white `<canvas>` element at the bottom right.
const PLAIN_BODY: &str = "<body style='margin:0;font:160px/1 monospace'>\
    <span style='position:fixed;left:10px;top:10px'>&#x2588;</span>\
    <canvas width=100 height=100 style='position:fixed;right:0;bottom:0'></canvas>\
    <script>const c=document.querySelector('canvas').getContext('2d');\
    c.fillStyle='white';c.fillRect(0,0,100,100)</script></body>";

/// Where the canvas color shows: the top right, away from the glyph and the `<canvas>`.
const CANVAS_POINT: (u32, u32) = (450, 50);
/// Inside the `<canvas>` element.
const IMAGE_POINT: (u32, u32) = (450, 450);
/// Inside the element that opts out in the element case.
const KEPT_POINT: (u32, u32) = (325, 325);
/// The area the glyph is drawn in.
const GLYPH_AREA: (u32, u32, u32, u32) = (10, 10, 200, 200);
/// Inside the border of the element that opts out in the element case.
const KEPT_BORDER_POINT: (u32, u32) = (302, 325);
/// Inside the card in the gradient case, away from its glyph.
const CARD_POINT: (u32, u32) = (340, 340);
/// The area the card's glyph is drawn in.
const CARD_GLYPH_AREA: (u32, u32, u32, u32) = (250, 250, 300, 300);

/// A new URL for a page with `head` and [`PLAIN_BODY`].
fn page(head: &str) -> Url {
    static PAGES: AtomicUsize = AtomicUsize::new(0);
    let number = PAGES.fetch_add(1, Ordering::Relaxed);
    // `#` would end the data URL's content and start its fragment.
    let html = format!("<!DOCTYPE html><!--{number}-->{head}{PLAIN_BODY}").replace('#', "%23");
    Url::parse(&format!("data:text/html,{html}")).unwrap()
}

fn luminance(pixel: [u8; 3]) -> f32 {
    let linear = |component: u8| {
        let component = f32::from(component) / 255.0;
        if component <= 0.040_45 {
            component / 12.92
        } else {
            ((component + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(pixel[0]) + 0.7152 * linear(pixel[1]) + 0.0722 * linear(pixel[2])
}

fn contrast(first: [u8; 3], second: [u8; 3]) -> f32 {
    let (first, second) = (luminance(first), luminance(second));
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

struct Painted {
    canvas: [u8; 3],
    ink: [u8; 3],
    image: [u8; 3],
    screenshot: RgbaImage,
}

impl Painted {
    fn of(image: &RgbaImage) -> Self {
        let pixel = |(x, y): (u32, u32)| {
            let pixel = image.get_pixel(x, y).0;
            [pixel[0], pixel[1], pixel[2]]
        };
        let canvas = pixel(CANVAS_POINT);

        // The glyph pixel that differs most from the canvas is solid ink.
        let (left, top, right, bottom) = GLYPH_AREA;
        let ink = (left..right)
            .flat_map(|x| (top..bottom).map(move |y| (x, y)))
            .map(pixel)
            .max_by(|a, b| contrast(*a, canvas).total_cmp(&contrast(*b, canvas)))
            .unwrap();
        Self {
            canvas,
            ink,
            image: pixel(IMAGE_POINT),
            screenshot: image.clone(),
        }
    }

    fn pixel(&self, (x, y): (u32, u32)) -> [u8; 3] {
        let pixel = self.screenshot.get_pixel(x, y).0;
        [pixel[0], pixel[1], pixel[2]]
    }

    /// The largest contrast between a pixel in `area` and `background`.
    fn max_contrast(
        &self,
        (left, top, right, bottom): (u32, u32, u32, u32),
        background: [u8; 3],
    ) -> f32 {
        (left..right)
            .flat_map(|x| (top..bottom).map(move |y| (x, y)))
            .map(|point| contrast(self.pixel(point), background))
            .fold(0.0, f32::max)
    }

    /// Whether the card of the gradient case is dark, with readable text.
    fn has_readable_dark_card(&self) -> bool {
        let card = self.pixel(CARD_POINT);
        luminance(card) < 0.03 && self.max_contrast(CARD_GLYPH_AREA, card) >= 4.5
    }

    fn is_authored_light(&self) -> bool {
        self.canvas == [255, 255, 255] && luminance(self.ink) < 0.01
    }

    fn is_adapted(&self) -> bool {
        luminance(self.canvas) < 0.03 &&
            luminance(self.ink) > luminance(self.canvas) &&
            contrast(self.ink, self.canvas) >= 4.5
    }

    fn describe(&self) -> String {
        format!(
            "canvas {:?}, ink {:?}, <canvas> {:?}",
            self.canvas, self.ink, self.image
        )
    }
}

struct Harness {
    servo_test: ServoTest,
    webview: WebView,
}

impl Harness {
    fn new() -> Self {
        let servo_test = ServoTest::new();
        let webview = WebViewBuilder::new(servo_test.servo(), servo_test.rendering_context.clone())
            .delegate(Rc::new(WebViewDelegateImpl::default()))
            .url(Url::parse("about:blank").unwrap())
            .build();
        let blank = webview.clone();
        servo_test.spin(move || {
            blank.url().is_none_or(|url| url.as_str() != "about:blank") ||
                blank.load_status() != LoadStatus::Complete
        });
        Self {
            servo_test,
            webview,
        }
    }

    /// Loads `url` and waits for it to finish loading. Every case's URL differs from the
    /// previous one, so a new URL means the load committed.
    fn load(&self, url: Url) {
        let previous = self.webview.url();
        assert_ne!(previous.as_ref(), Some(&url), "each load needs a new URL");
        self.webview.load(url);
        let webview = self.webview.clone();
        let deadline = Instant::now() + LOAD_TIMEOUT;
        self.servo_test.spin(move || {
            assert!(Instant::now() < deadline, "the page did not load");
            webview.url() == previous || webview.load_status() != LoadStatus::Complete
        });
    }

    /// Takes screenshots until one satisfies `expected`, and returns the last one.
    fn painted_when(&self, expected: impl Fn(&Painted) -> bool) -> Painted {
        let mut painted = self.painted();
        for _ in 1..MAX_SCREENSHOTS {
            if expected(&painted) {
                break;
            }
            painted = self.painted();
        }
        painted
    }

    fn assert_authored_light(&self, case: &str) {
        let painted = self.painted_when(Painted::is_authored_light);
        assert!(
            painted.is_authored_light(),
            "{case}: {}",
            painted.describe()
        );
        assert_eq!(painted.image, [255, 255, 255], "{case}: <canvas>");
    }

    fn assert_adapted(&self, case: &str) {
        let painted = self.painted_when(Painted::is_adapted);
        assert!(painted.is_adapted(), "{case}: {}", painted.describe());
        assert_eq!(
            painted.image,
            [255, 255, 255],
            "{case}: <canvas> keeps its pixels"
        );
    }

    fn painted(&self) -> Painted {
        let screenshot: Rc<RefCell<Option<RgbaImage>>> = Default::default();
        let waiting = Rc::new(Cell::new(true));
        let (result, done) = (screenshot.clone(), waiting.clone());
        self.webview.take_screenshot(None, move |image| {
            *result.borrow_mut() = Some(image.expect("screenshot"));
            done.set(false);
        });
        self.servo_test.spin(|| waiting.get());
        let image = screenshot.borrow_mut().take().unwrap();
        Painted::of(&image)
    }

    fn evaluate(&self, script: &str) -> JSValue {
        evaluate_javascript(&self.servo_test, self.webview.clone(), script).unwrap()
    }
}

#[test]
fn test_adaptive_dark_mode() {
    let harness = Harness::new();

    // Off by default.
    harness.load(page(""));
    harness.assert_authored_light("default");

    // On: the canvas turns dark, the text light, the <canvas> keeps its pixels, and
    // scripts read the authored colors.
    harness.webview.set_adaptive_dark_mode(true);
    harness.load(page(""));
    harness.assert_adapted("unstyled page");
    assert_eq!(
        harness.evaluate("getComputedStyle(document.body).color"),
        JSValue::String("rgb(0, 0, 0)".into())
    );

    // An authored white background turns dark too.
    harness.load(page("<style>body{background:white;color:#222}</style>"));
    harness.assert_adapted("white background");
    assert_eq!(
        harness.evaluate("getComputedStyle(document.body).backgroundColor"),
        JSValue::String("rgb(255, 255, 255)".into())
    );

    // A page under a repeated background image keeps its colors, whatever the image shows.
    harness.load(page(
        "<style>body{background:white url(data:image/gif;base64,\
         R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7)}</style>",
    ));
    harness.assert_authored_light("repeated background image");

    // A dark page keeps its colors.
    harness.load(page(
        "<style>html{background:#202124;color:#e8eaed}</style>",
    ));
    let dark = harness.painted_when(|painted| luminance(painted.ink) > 0.7);
    assert_eq!(dark.canvas, [0x20, 0x21, 0x24], "dark page canvas");
    assert!(luminance(dark.ink) > 0.7, "dark page ink {:?}", dark.ink);

    // Pages that declare dark support or `only` are painted as authored.
    for (case, head) in [
        (
            "CSS light only",
            "<style>:root{color-scheme:light only}</style>",
        ),
        (
            "CSS only light",
            "<style>:root{color-scheme:only light}</style>",
        ),
        (
            "CSS light dark",
            "<style>:root{color-scheme:light dark}</style>",
        ),
        (
            "metadata light only",
            "<meta name=color-scheme content='light only'>",
        ),
        (
            "metadata light dark",
            "<meta name=color-scheme content='light dark'>",
        ),
        (
            "CSS body light only",
            "<style>body{color-scheme:light only}</style>",
        ),
        (
            "CSS body light dark",
            "<style>body{color-scheme:light dark}</style>",
        ),
    ] {
        harness.load(page(head));
        harness.assert_authored_light(case);
    }

    // Metadata that is not a valid `color-scheme` value does not opt out.
    for (case, content) in [
        ("metadata only", "only"),
        ("metadata light only dark", "light only dark"),
        ("metadata normal dark", "normal dark"),
    ] {
        harness.load(page(&format!(
            "<meta name=color-scheme content='{content}'>"
        )));
        harness.assert_adapted(case);
    }

    // Removing opt-out metadata from a painted page lets adaptive dark mode paint it.
    harness.load(page("<meta name=color-scheme content='light only'>"));
    harness.evaluate("document.querySelector('meta').remove()");
    harness.assert_adapted("metadata removed");

    // An element that opts out keeps its colors, border included, inside a darkened page.
    harness.load(page(
        "<style>#keep{position:fixed;left:300px;top:300px;width:50px;height:50px;\
         background:white;border:5px solid black;color-scheme:only light}</style>\
         <div id=keep></div>",
    ));
    harness.assert_adapted("page around an opted-out element");
    let painted = harness.painted();
    assert_eq!(
        painted.pixel(KEPT_POINT),
        [255, 255, 255],
        "opted-out element"
    );
    assert_eq!(
        painted.pixel(KEPT_BORDER_POINT),
        [0, 0, 0],
        "opted-out element's border"
    );

    // A border outside a box's background (`background-clip: padding-box`) is painted over
    // what is behind the box: here a dark page that kept its color, so the border keeps
    // its color too.
    harness.load(page(
        "<style>html{background:#111}#box{position:fixed;left:300px;top:300px;width:50px;\
         height:50px;background:white;background-clip:padding-box;border:5px solid #444}\
         </style><div id=box></div>",
    ));
    let painted = harness.painted_when(|painted| luminance(painted.pixel(KEPT_POINT)) < 0.03);
    assert_eq!(
        painted.pixel(KEPT_BORDER_POINT),
        [0x44, 0x44, 0x44],
        "border outside a padding-box background, over a dark page"
    );

    // Text that opts out but paints no background of its own follows what is painted
    // behind it, which got darker.
    harness.load(page("<style>span{color-scheme:only light}</style>"));
    harness.assert_adapted("opted-out text without a background");

    // A light gradient on a dark page gets darker like a background color, and the text
    // over it lighter.
    harness.load(page(
        "<style>html{background:#202124}#card{position:fixed;left:250px;top:250px;\
         width:100px;height:100px;font-size:40px;background:linear-gradient(white,white)}\
         </style><div id=card>&#x2588;</div>",
    ));
    let painted = harness.painted_when(Painted::has_readable_dark_card);
    assert!(
        painted.has_readable_dark_card(),
        "text over a light gradient: card {:?}, text contrast {}",
        painted.pixel(CARD_POINT),
        painted.max_contrast(CARD_GLYPH_AREA, painted.pixel(CARD_POINT))
    );

    // The shell's white does not show through a translucent canvas or one with only a
    // small image.
    for (case, head) in [
        (
            "faint translucent background",
            "<style>body{background:rgba(0,0,0,0.03)}</style>",
        ),
        (
            "half transparent white background",
            "<style>body{background:rgba(255,255,255,0.5)}</style>",
        ),
        (
            "small background image",
            "<style>body{background:url(data:image/gif;base64,\
             R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7) no-repeat}</style>",
        ),
    ] {
        harness.load(page(head));
        harness.assert_adapted(case);
    }

    // Under a dark theme, as in an embedder with a dark appearance, the root's light
    // color-scheme hint does not opt pages out, but an opt-out still applies.
    harness.webview.notify_theme_change(Theme::Dark);
    harness.load(page(""));
    harness.assert_adapted("unstyled page under a dark theme");
    harness.load(page("<meta name=color-scheme content='light only'>"));
    harness.assert_authored_light("metadata light only under a dark theme");

    // Turning it off repaints the current page as authored.
    harness.load(page(""));
    harness.assert_adapted("before turning off");
    harness.webview.set_adaptive_dark_mode(false);
    harness.assert_authored_light("turned off");
}
