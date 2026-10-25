# Downstream changes

Every change `combray/0.N` carries on top of its upstream release: why Combray needs
it, how to check it, and when upstream makes it redundant. Update this list in the
same PR as the change: add an entry with each downstream change, and remove the entry
when an upstream merge drops the change. Each upstream merge reviews every entry (see
[AGENTS.md](AGENTS.md)). Describe changes through engine behavior and synthetic test
cases; never name a website.

Entries whose check says "no engine-side test yet" predate the regression-test rule
in AGENTS.md. They are the exception, not the norm: add a WPT test under
`tests/wpt/mozilla/tests/` for each of them before the next upstream merge, and
record its command here.

| Change | Commits | Why | Check | Drop when |
|---|---|---|---|---|
| Downstream repository setup | `d65da5c839c`, `a5c85183440` | Removes upstream CI workflows, CODEOWNERS and dependabot, which can contact other projects; adds `AGENTS.md`, the README section and this file | `.github/workflows/`, `.github/CODEOWNERS` and `.github/dependabot.yml` stay absent after every upstream merge | Never |
| Embedder API: top-level document identity | `f860cdf98ec` | Combray's native bridge tags callbacks with the reporting document and evaluates scripts only in a still-active document. The v0.7.0 merge dropped its global-scope guard: upstream no longer asserts there | Combray's Engine workflow (native builds and device checks) | Upstream offers an equivalent API and Combray has moved to it |
| Paint: log WebRender render errors | `70b288de85e` | A failed render silently left the previous frame on screen | None (logging only) | Upstream handles the result of `Renderer::render` |
| Net: persist profile state on request, atomically | `718bb542539`, `15b92fc4c9d` | Android kills apps without a clean exit, which lost cookies, HSTS and auth state; in-place writes could truncate profile files | Combray's Engine workflow (persistence device checks) | Upstream persists on demand and writes profile files atomically |
| DOM: `:has()` in `querySelector`, `matches` and `closest` | `97be6cb1fb5` | DOM queries with `:has()` threw; stylesheet `:has()` stays disabled until Servo wires relative invalidation | No engine-side test yet; Combray's device checks cover the startup case | Upstream enables `:has()` for DOM queries |
| Paint: chain input scrolls from fixed layers to the viewport | `4a294fff0ea`, `1a96dd8d50b` | Fixed-position descendants sit beside the viewport in the spatial tree, so an unconsumed wheel, touch pan or fling over a fixed layer (a closed drawer, a sticky bar) never scrolled the page | `cargo test --profile medium -p servo-paint-api --test compositor` (`test_scroll_tree_chain_from_fixed_drawer_to_viewport`, `test_touch_scroll_tree_chain_from_fixed_drawer_to_viewport`) | Upstream chains input scrolls from fixed layers to their document's viewport |
| Script: keep default page colors light until a page opts into dark | `b7687abc34c`, `546db2b2db1` | With a dark system appearance, pages without their own colors got pale default text on a white canvas; the root now gets a light color-scheme hint until metadata or an author style opts into dark, and rematches when metadata changes | Combray's Engine workflow (painted-color device regression); no engine-side test yet | Upstream keeps unstyled pages light until they opt into a dark color scheme |
| WebDriver: listen on loopback only | `1f380c6b3fc` | The site check runs servoshell with WebDriver on a home network; `0.0.0.0` exposed it to the LAN | `cargo test --profile medium -p servo-webdriver-server --lib` (`listens_on_loopback_only`) | Upstream binds loopback |
