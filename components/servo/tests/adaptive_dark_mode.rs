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
use servo::{JSValue, LoadStatus, PrefValue, Theme, WebView, WebViewBuilder};
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

/// Where the image of the image cases is: 100 by 100 pixels, clear of the glyph and the
/// `<canvas>`.
const IMAGE_STYLE: &str = "position:fixed;left:250px;top:250px;width:100px;height:100px";
/// Inside the artwork on the left of those images.
const ARTWORK_POINT: (u32, u32) = (265, 300);
/// Inside those images, to the right of their artwork.
const BESIDE_ARTWORK_POINT: (u32, u32) = (330, 300);

/// Inside the glyph of the caption that the caption case puts over its image.
const CAPTION_POINT: (u32, u32) = (260, 270);

/// The middle of an element at [`IMAGE_STYLE`], where a radio button has its bullet.
const BULLET_POINT: (u32, u32) = (300, 300);

/// A PNG of 10 by 10 pixels: four black columns on the left, the rest transparent.
const DARK_ON_CLEAR_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAoAAAAKCAYAAACNMs\
    +9AAAAFUlEQVR42mNgYGD4j4ZxglGFdFQIAI3dJ9mgfgNXAAAAAElFTkSuQmCC";

/// A PNG of 8 by 8 pixels: 32 different dark colors on the left half, the rest transparent.
/// So many colors make a picture, not a logo.
const DARK_PICTURE_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76L\
    AAAASUlEQVR42m3OMREAMQACwZRIQAIS8G8sufofZra7giPpGkFxvpMJENQrCAGCZgUlQNCuQOaD+WA+eH0gQFCvIAQI\
    mhWUAEH7Dx4hginhq6osBAAAAABJRU5ErkJggg==";

/// A PNG of 8 by 8 opaque pixels, all `rgb(220, 220, 220)`: a light picture.
const LIGHT_PICTURE_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSnc\
    AAAAD0lEQVR42mO4gwMwDC0JAHiIpQFQusoIAAAAAElFTkSuQmCC";

/// The URL of an SVG image of 100 by 100 pixels with `shapes`. Their colors need names:
/// `#` would end the URL.
fn svg(shapes: &str) -> String {
    format!(
        "data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'>\
         {shapes}</svg>"
    )
}

/// An `<img>` of `source` at [`IMAGE_STYLE`].
fn image(source: &str) -> String {
    format!("<img style='{IMAGE_STYLE}' src=\"{source}\">")
}

/// An element at [`IMAGE_STYLE`] with `source` as its background image, repeated as `repeat`
/// says. The image's quotes would end a `style` attribute.
fn background_image(source: &str, repeat: &str) -> String {
    format!(
        "<style>#icon{{{IMAGE_STYLE};background:url(\"{source}\") {repeat}}}</style>\
         <div id=icon></div>"
    )
}

/// Black artwork on the left of a transparent image.
const DARK_ARTWORK: &str = "<rect width='40' height='100'/>";

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

    /// Whether the artwork of the image case is light on a part that is as dark as the page.
    fn has_light_artwork(&self) -> bool {
        let beside = self.pixel(BESIDE_ARTWORK_POINT);
        luminance(self.pixel(ARTWORK_POINT)) > 0.5 &&
            beside
                .iter()
                .zip(self.canvas)
                .all(|(a, b)| a.abs_diff(b) <= 2)
    }

    fn is_authored_light(&self) -> bool {
        self.canvas == [255, 255, 255] && luminance(self.ink) < 0.01
    }

    /// Whether the page shows the default colors of a dark color scheme: Stylo's `Canvas`
    /// and `CanvasText` system colors.
    fn is_dark_scheme(&self) -> bool {
        self.canvas == [30, 30, 30] && self.ink == [232, 232, 232]
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

    fn assert_dark_scheme(&self, case: &str) {
        let painted = self.painted_when(Painted::is_dark_scheme);
        assert!(painted.is_dark_scheme(), "{case}: {}", painted.describe());
    }

    /// Sets the color the shell shows where no page paints.
    fn set_shell_background(&self, color: [f64; 4]) {
        self.servo_test
            .servo()
            .set_preference("shell_background_color_rgba", PrefValue::from(color));
    }

    fn assert_canvas(&self, case: &str, color: [u8; 3]) {
        let painted = self.painted_when(|painted| painted.canvas == color);
        assert_eq!(painted.canvas, color, "{case}");
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
    // The bullet of a checked radio button is black whatever the page's color for inputs.
    harness.load(page(&format!(
        "<input type=radio checked style='{IMAGE_STYLE};color:white'>"
    )));
    let has_black_bullet = |painted: &Painted| luminance(painted.pixel(BULLET_POINT)) < 0.01;
    let painted = harness.painted_when(has_black_bullet);
    assert!(
        has_black_bullet(&painted),
        "authored radio bullet: {:?}",
        painted.pixel(BULLET_POINT)
    );

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

    // A dark logo on a transparent background would vanish on the darkened page: its
    // lightness is inverted. So is that of a dark icon in a background that it does not fill.
    let dark_svg = svg(DARK_ARTWORK);
    for (case, markup) in [
        ("dark SVG image", image(&dark_svg)),
        ("dark PNG image", image(DARK_ON_CLEAR_PNG)),
        (
            "dark background icon",
            background_image(&dark_svg, "no-repeat"),
        ),
    ] {
        harness.load(page(&markup));
        let painted = harness.painted_when(Painted::has_light_artwork);
        assert!(
            painted.has_light_artwork(),
            "{case}: artwork {:?}, beside it {:?}, {}",
            painted.pixel(ARTWORK_POINT),
            painted.pixel(BESIDE_ARTWORK_POINT),
            painted.describe()
        );
    }

    // Scripts still read the image's own pixels.
    harness.load(page(&image(DARK_ON_CLEAR_PNG)));
    assert_eq!(
        harness.evaluate(
            "(() => { const context = document.createElement('canvas').getContext('2d');\
             context.drawImage(document.querySelector('img'), 0, 0);\
             return context.getImageData(1, 5, 1, 1).data.join() })()"
        ),
        JSValue::String("0,0,0,255".into())
    );

    // A black and white logo in a white box: the box turns into the page's dark too.
    harness.load(page(&image(&svg(
        "<rect width='100' height='100' fill='white'/><rect x='10' y='10' width='15' height='80'/>",
    ))));
    let painted = harness.painted_when(Painted::has_light_artwork);
    assert!(
        painted.has_light_artwork(),
        "black and white logo: artwork {:?}, beside it {:?}, {}",
        painted.pixel(ARTWORK_POINT),
        painted.pixel(BESIDE_ARTWORK_POINT),
        painted.describe()
    );

    // Images that show on a dark page, pictures, and images that may be codes for a
    // scanner keep their pixels. `artwork` is the darkest and lightest their artwork may be.
    let half_dark =
        svg("<rect width='100' height='100' fill='white'/><rect width='50' height='100'/>");
    for (case, markup, artwork) in [
        (
            "white SVG image",
            image(&svg("<rect width='40' height='100' fill='white'/>")),
            0.99..=1.0,
        ),
        (
            "red SVG image",
            image(&svg("<rect width='40' height='100' fill='red'/>")),
            0.2..=0.22,
        ),
        ("dark picture", image(DARK_PICTURE_PNG), 0.0..=0.05),
        ("half dark image", image(&half_dark), 0.0..=0.0),
        (
            "dark background image that fills its area",
            background_image(&dark_svg, "repeat"),
            0.0..=0.0,
        ),
    ] {
        harness.load(page(&markup));
        // The image shows once its artwork differs from the page around it.
        let painted =
            harness.painted_when(|painted| painted.pixel(ARTWORK_POINT) != painted.canvas);
        let painted_artwork = luminance(painted.pixel(ARTWORK_POINT));
        assert!(
            artwork.contains(&painted_artwork),
            "{case}: artwork {:?}, {}",
            painted.pixel(ARTWORK_POINT),
            painted.describe()
        );
        assert!(painted.is_adapted(), "{case}: {}", painted.describe());
    }

    // A dark image on a page that is dark as authored, or in an element that keeps its
    // light background, has the backdrop it was drawn for.
    for (case, head) in [
        (
            "dark image on a dark page",
            format!("<style>html{{background:#333}}</style>{}", image(&dark_svg)),
        ),
        (
            "dark image in an opted-out element",
            format!(
                "<div style='{IMAGE_STYLE};color-scheme:light only;background:white'>\
                 <img style='width:100px;height:100px' src=\"{dark_svg}\"></div>"
            ),
        ),
    ] {
        harness.load(page(&head));
        let is_painted =
            |painted: &Painted| painted.pixel(ARTWORK_POINT) != painted.pixel(BESIDE_ARTWORK_POINT);
        let painted = harness.painted_when(is_painted);
        assert_eq!(
            painted.pixel(ARTWORK_POINT),
            [0, 0, 0],
            "{case}: {}",
            painted.describe()
        );
    }

    // White text positioned over a picture keeps its color: the page behind both got dark,
    // but the text was never drawn for the page.
    harness.load(page(&format!(
        "{}<span style='position:fixed;left:250px;top:250px;font-size:40px;color:white'>\
         &#x2588;</span>",
        image(&svg("<rect width='100' height='100' fill='red'/>"))
    )));
    let is_white = |painted: &Painted| painted.pixel(CAPTION_POINT) == [255, 255, 255];
    let painted = harness.painted_when(is_white);
    assert!(
        is_white(&painted),
        "white text over a picture: {:?}, {}",
        painted.pixel(CAPTION_POINT),
        painted.describe()
    );

    // What lies over an opaque picture that kept its pixels was drawn for the picture:
    // dark text and a dark icon keep their colors over a light one.
    let picture = format!(
        "<img style='position:fixed;left:240px;top:240px;width:140px;height:140px' \
         src='{LIGHT_PICTURE_PNG}'>"
    );
    harness.load(page(&format!(
        "{picture}<span style='position:fixed;left:250px;top:250px;font-size:40px;\
         color:black'>&#x2588;</span>"
    )));
    let is_black = |painted: &Painted| painted.pixel(CAPTION_POINT) == [0, 0, 0];
    let painted = harness.painted_when(is_black);
    assert!(
        is_black(&painted),
        "black text over a light picture: {:?}, {}",
        painted.pixel(CAPTION_POINT),
        painted.describe()
    );
    harness.load(page(&format!("{picture}{}", image(&svg(DARK_ARTWORK)))));
    let has_dark_artwork = |painted: &Painted| {
        luminance(painted.pixel(ARTWORK_POINT)) < 0.01 &&
            painted.pixel(BESIDE_ARTWORK_POINT) == [220, 220, 220]
    };
    let painted = harness.painted_when(has_dark_artwork);
    assert!(
        has_dark_artwork(&painted),
        "dark icon over a light picture: {:?} beside {:?}, {}",
        painted.pixel(ARTWORK_POINT),
        painted.pixel(BESIDE_ARTWORK_POINT),
        painted.describe()
    );

    // A box with a background of its own hides the picture under it: the box turns dark
    // like the page, and its text light.
    harness.load(page(&format!(
        "{picture}<div style='{IMAGE_STYLE};background:white;color:black;font-size:40px;\
         line-height:1'>&#x2588;</div>"
    )));
    let has_light_text_in_dark_box = |painted: &Painted| {
        luminance(painted.pixel(CAPTION_POINT)) > 0.5 &&
            luminance(painted.pixel(BESIDE_ARTWORK_POINT)) < 0.03
    };
    let painted = harness.painted_when(has_light_text_in_dark_box);
    assert!(
        has_light_text_in_dark_box(&painted),
        "white box over a picture: text {:?} in {:?}, {}",
        painted.pixel(CAPTION_POINT),
        painted.pixel(BESIDE_ARTWORK_POINT),
        painted.describe()
    );

    // The bullet of a checked radio button is a mark like text: it turns light on the
    // darkened control.
    harness.load(page(&format!(
        "<input type=radio checked style='{IMAGE_STYLE}'>"
    )));
    let has_light_bullet = |painted: &Painted| luminance(painted.pixel(BULLET_POINT)) > 0.5;
    let painted = harness.painted_when(has_light_bullet);
    assert!(
        has_light_bullet(&painted),
        "radio bullet: {:?}, {}",
        painted.pixel(BULLET_POINT),
        painted.describe()
    );

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

    // A page that uses a dark color scheme and sets no colors has that scheme's canvas
    // under its light default text, not the shell's white. The theme is still dark, so a
    // page whose CSS supports both schemes uses the dark one. Adaptive dark mode leaves
    // such pages alone.
    for adaptive_dark_mode in [false, true] {
        harness.webview.set_adaptive_dark_mode(adaptive_dark_mode);
        for (case, head) in [
            ("metadata dark", "<meta name=color-scheme content=dark>"),
            ("CSS dark", "<style>:root{color-scheme:dark}</style>"),
            (
                "CSS light dark",
                "<style>:root{color-scheme:light dark}</style>",
            ),
        ] {
            harness.load(page(head));
            harness.assert_dark_scheme(&format!("{case}, adaptive dark mode {adaptive_dark_mode}"));
        }
    }
    harness.webview.set_adaptive_dark_mode(false);

    // That canvas is the `Canvas` system color, as an element paints it.
    harness.load(page(
        "<style>:root{color-scheme:dark}</style><div style='position:fixed;left:300px;\
         top:300px;width:50px;height:50px;background:Canvas'></div>",
    ));
    let painted = harness.painted_when(Painted::is_dark_scheme);
    assert!(painted.is_dark_scheme(), "{}", painted.describe());
    assert_eq!(painted.pixel(KEPT_POINT), painted.canvas, "Canvas");

    // Under a light theme, a page that supports only the dark scheme is still dark.
    harness.webview.notify_theme_change(Theme::Light);
    harness.load(page("<style>:root{color-scheme:dark}</style>"));
    harness.assert_dark_scheme("CSS dark under a light theme");

    // The shell's background color is the embedder's. It does not show through a page
    // that sets no colors, light or darkened.
    harness.set_shell_background([0.0, 0.0, 1.0, 1.0]);
    harness.load(page(""));
    harness.assert_authored_light("over a blue shell");
    harness.webview.set_adaptive_dark_mode(true);
    harness.assert_adapted("darkened over a blue shell");
    harness.webview.set_adaptive_dark_mode(false);

    // It shows where no page paints, such as a document without a root element, and
    // follows the preference while Servo runs.
    harness.evaluate("document.documentElement.remove()");
    harness.assert_canvas("blue shell", [0, 0, 255]);
    harness.set_shell_background([1.0, 0.0, 0.0, 1.0]);
    harness.assert_canvas("red shell", [255, 0, 0]);
    harness.set_shell_background([1.0, 1.0, 1.0, 1.0]);
}
