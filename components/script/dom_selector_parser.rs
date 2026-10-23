/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Selector parsing for stateless DOM queries, independently of stylesheet support.

use cssparser::{CowRcStr, Parser as CssParser, ParserInput, SourceLocation};
use selectors::parser::{ParseRelative, Parser, SelectorList};
use style::selector_parser::{NonTSPseudoClass, PseudoElement, SelectorImpl, SelectorParser};
use style::stylesheets::{Namespaces, Origin, UrlExtraData};
use style::{Namespace, Prefix};
use style_traits::{ParseError, StyleParseErrorKind};

/// Parse DOM selectors with native :has() matching, which needs no style invalidation.
/// Stylesheets and CSS.supports must keep using Stylo's preference-gated parser.
pub(crate) fn parse_dom_selectors<'i>(
    input: &'i str,
    url_data: &UrlExtraData,
) -> Result<SelectorList<SelectorImpl>, ParseError<'i>> {
    let namespaces = Namespaces::default();
    let parser = DomSelectorParser(SelectorParser {
        stylesheet_origin: Origin::Author,
        namespaces: &namespaces,
        url_data,
        for_supports_rule: false,
    });
    let mut input = ParserInput::new(input);
    SelectorList::parse(&parser, &mut CssParser::new(&mut input), ParseRelative::No)
}

struct DomSelectorParser<'a>(SelectorParser<'a>);

// Forward every other hook: DOM selectors keep Stylo's namespace and pseudo-class rules.
// Recheck this list when updating the pinned selectors crate; its hooks have default bodies.
impl<'i> Parser<'i> for DomSelectorParser<'_> {
    type Impl = SelectorImpl;
    type Error = StyleParseErrorKind<'i>;

    fn parse_has(&self) -> bool {
        true
    }

    fn parse_slotted(&self) -> bool {
        self.0.parse_slotted()
    }

    fn parse_part(&self) -> bool {
        self.0.parse_part()
    }

    fn parse_nth_child_of(&self) -> bool {
        self.0.parse_nth_child_of()
    }

    fn parse_is_and_where(&self) -> bool {
        self.0.parse_is_and_where()
    }

    fn parse_parent_selector(&self) -> bool {
        self.0.parse_parent_selector()
    }

    fn is_is_alias(&self, name: &str) -> bool {
        self.0.is_is_alias(name)
    }

    fn parse_host(&self) -> bool {
        self.0.parse_host()
    }

    fn allow_forgiving_selectors(&self) -> bool {
        self.0.allow_forgiving_selectors()
    }

    fn parse_non_ts_pseudo_class(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<NonTSPseudoClass, ParseError<'i>> {
        self.0.parse_non_ts_pseudo_class(location, name)
    }

    fn parse_non_ts_functional_pseudo_class<'t>(
        &self,
        name: CowRcStr<'i>,
        parser: &mut CssParser<'i, 't>,
        after_part: bool,
    ) -> Result<NonTSPseudoClass, ParseError<'i>> {
        self.0
            .parse_non_ts_functional_pseudo_class(name, parser, after_part)
    }

    fn parse_pseudo_element(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<PseudoElement, ParseError<'i>> {
        self.0.parse_pseudo_element(location, name)
    }

    fn parse_functional_pseudo_element<'t>(
        &self,
        name: CowRcStr<'i>,
        arguments: &mut CssParser<'i, 't>,
    ) -> Result<PseudoElement, ParseError<'i>> {
        self.0.parse_functional_pseudo_element(name, arguments)
    }

    fn default_namespace(&self) -> Option<Namespace> {
        self.0.default_namespace()
    }

    fn namespace_for_prefix(&self, prefix: &Prefix) -> Option<Namespace> {
        self.0.namespace_for_prefix(prefix)
    }
}
