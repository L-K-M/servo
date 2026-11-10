/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Fallback templates for `grid-template-*: subgrid` items.
//!
//! Taffy has no subgrid support, so without a fallback the tracks of a
//! subgrid item come out as implicit auto tracks sized to its content, and
//! wide content widens the item and the page far past the viewport. This
//! module approximates subgrid by resolving the slice of the parent grid's
//! track list that the item spans, together with the line names covering
//! that slice, so the item's children lay out against the parent's tracks.
//!
//! A subgrid's own resolved tracks serve as the parent data for subgrids
//! nested inside it, and [`SubgridTemplates::re_resolve`] re-derives the
//! slice when a restyle changes the styles it was resolved from. The
//! approximation does not resolve parent `repeat(auto-fill, ...)` lists or
//! auto-repeating `<track-repeat>` groups; those keep the previous behavior
//! (implicit auto tracks), as does inheriting the parent grid's gutters.
//! The subgrid's own `subgrid <line-name-list>` names override the sliced
//! parent names when they are plain name groups.

use malloc_size_of_derive::MallocSizeOf;
use style::OwnedSlice;
use style::values::CustomIdent;
use style::values::computed::{GridLine, GridTemplateComponent, Integer, LengthPercentage};
use style::values::generics::grid::{LineNameList, LineNameListValue, RepeatCount, TrackListValue};
use stylo_atoms::atom;

/// The most expanded track values one axis may materialize from
/// `<track-repeat>` groups. A larger count (a page can ask for
/// `repeat(2147483647, ...)`) is not sliced here; the axis keeps the
/// previous behavior instead of allocating the whole list.
const MAX_EXPANDED_TRACKS: usize = 10_000;

/// One axis of grid tracks with the line names for their edges.
#[derive(Clone, Debug, MallocSizeOf)]
pub(crate) struct SubgridAxisTemplate {
    // The stylo values use stylo's separate malloc trait; the layout tree is
    // rebuilt per layout pass, so they are not measured here.
    #[ignore_malloc_size_of = "stylo track values use a separate malloc trait"]
    tracks: Vec<TrackListValue<LengthPercentage, Integer>>,
    #[ignore_malloc_size_of = "stylo line names use a separate malloc trait"]
    line_names: OwnedSlice<OwnedSlice<CustomIdent>>,
}

impl SubgridAxisTemplate {
    pub(crate) fn tracks(&self) -> &[TrackListValue<LengthPercentage, Integer>] {
        &self.tracks
    }

    pub(crate) fn line_names(&self) -> &OwnedSlice<OwnedSlice<CustomIdent>> {
        &self.line_names
    }
}

/// Fallback templates for a subgrid item. Each axis is `Some` only when it
/// is `subgrid` and its slice could be resolved from the parent's tracks.
#[derive(Debug, Default, MallocSizeOf)]
pub(crate) struct SubgridTemplates {
    /// The slice of the parent's tracks this item spans.
    pub columns: Option<SubgridAxisTemplate>,
    pub rows: Option<SubgridAxisTemplate>,
    /// The parent axis data the slices were resolved from, kept so a
    /// restyle can re-resolve them against changed styles.
    parent_columns: Option<SubgridAxisTemplate>,
    parent_rows: Option<SubgridAxisTemplate>,
}

impl SubgridTemplates {
    /// Re-resolve this item's slices after its own style changed, against
    /// the parent data they were originally resolved from.
    pub(crate) fn re_resolve(
        &self,
        item: &style::properties::ComputedValues,
    ) -> Option<SubgridTemplates> {
        resolve_for_item(
            self.parent_columns.as_ref(),
            self.parent_rows.as_ref(),
            item,
        )
    }
}

/// The track and name data one axis of a grid container exposes to its grid
/// items: its explicit track list (fixed `<track-repeat>` groups expanded),
/// or the axis's resolved fallback when the container is itself a subgrid.
/// Returns `None` for `none`, `masonry` and auto-repeating lists.
pub(crate) fn axis_source(
    component: &GridTemplateComponent,
    own_fallback: Option<&SubgridAxisTemplate>,
) -> Option<SubgridAxisTemplate> {
    match component {
        style::values::generics::grid::GenericGridTemplateComponent::TrackList(list) => {
            let mut tracks = Vec::new();
            let mut names = Some(list.line_names.clone());
            for value in list.values.iter() {
                match value {
                    TrackListValue::TrackSize(_) => tracks.push(value.clone()),
                    TrackListValue::TrackRepeat(repeat) => match repeat.count {
                        RepeatCount::Number(count) => {
                            let repeated = count.max(0) as usize * repeat.track_sizes.len().max(1);
                            if tracks.len() + repeated > MAX_EXPANDED_TRACKS {
                                return None;
                            }
                            names = None;
                            for _ in 0..count.max(0) {
                                for size in repeat.track_sizes.iter() {
                                    tracks.push(TrackListValue::TrackSize(size.clone()));
                                }
                            }
                        },
                        RepeatCount::AutoFill | RepeatCount::AutoFit => return None,
                    },
                }
            }
            if tracks.is_empty() {
                return None;
            }
            let line_names = names.unwrap_or_else(|| unnamed_line_names(tracks.len() + 1));
            Some(SubgridAxisTemplate { tracks, line_names })
        },
        style::values::generics::grid::GenericGridTemplateComponent::Subgrid(_) => {
            own_fallback.cloned()
        },
        style::values::generics::grid::GenericGridTemplateComponent::None => None,
        style::values::generics::grid::GenericGridTemplateComponent::Masonry => None,
    }
}

/// Resolve the fallback templates for one grid item whose parent grid
/// exposes `parent_columns` and `parent_rows`. Returns `None` when no axis
/// needs a fallback or none could be resolved.
pub(crate) fn resolve_for_item(
    parent_columns: Option<&SubgridAxisTemplate>,
    parent_rows: Option<&SubgridAxisTemplate>,
    item: &style::properties::ComputedValues,
) -> Option<SubgridTemplates> {
    let position = item.get_position();
    let mut templates = SubgridTemplates {
        parent_columns: parent_columns.cloned(),
        parent_rows: parent_rows.cloned(),
        ..Default::default()
    };

    if let style::values::generics::grid::GenericGridTemplateComponent::Subgrid(names) =
        &position.grid_template_columns
    {
        templates.columns = parent_columns.and_then(|parent| {
            resolve_axis(
                parent,
                names,
                (&position.grid_column_start, &position.grid_column_end),
            )
        });
    }
    if let style::values::generics::grid::GenericGridTemplateComponent::Subgrid(names) =
        &position.grid_template_rows
    {
        templates.rows = parent_rows.and_then(|parent| {
            resolve_axis(
                parent,
                names,
                (&position.grid_row_start, &position.grid_row_end),
            )
        });
    }

    (templates.columns.is_some() || templates.rows.is_some()).then_some(templates)
}

/// A resolved grid line position in the parent's track list. Lines are
/// indices in `0..=track_count`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum LineResolution {
    /// `auto`: no definite line, so only a span can constrain the range.
    Auto,
    /// A definite line index.
    Index(usize),
    /// A `span <integer>` value.
    Span(usize),
}

/// Resolve the fallback for one axis: the parent's tracks covered by the
/// item's placement, and the line names covering that slice.
fn resolve_axis(
    parent: &SubgridAxisTemplate,
    subgrid_names: &LineNameList<Integer>,
    placement: (&GridLine, &GridLine),
) -> Option<SubgridAxisTemplate> {
    let tracks = &parent.tracks;
    let track_count = tracks.len();
    if track_count == 0 {
        return None;
    }
    let occurrences = |ident: &style::Atom| -> Vec<usize> {
        parent
            .line_names
            .iter()
            .enumerate()
            .filter(|(_, set)| set.iter().any(|name| name.0 == *ident))
            .map(|(index, _)| index)
            .collect()
    };

    let start = resolve_line(placement.0, track_count, &occurrences);
    let end = resolve_line(placement.1, track_count, &occurrences);
    let (start_index, count) = span_and_offset(start, end, track_count);

    let tracks = tracks[start_index..start_index + count].to_vec();
    let mut line_names = parent.line_names[start_index..start_index + count + 1].to_vec();
    if let Some(own_names) = subgrid_line_names(subgrid_names, count) {
        line_names = own_names;
    }

    Some(SubgridAxisTemplate {
        tracks,
        line_names: line_names.into(),
    })
}

/// Resolve one grid placement line of the item's span against the parent's
/// track list. Ident lines resolve to their first matching line unless a
/// line number selects another occurrence; `is_span` wins over both.
fn resolve_line(
    line: &GridLine,
    track_count: usize,
    occurrences: &impl Fn(&style::Atom) -> Vec<usize>,
) -> LineResolution {
    if line.is_span {
        return LineResolution::Span(line.line_num.max(1) as usize);
    }
    if line.ident.0 != atom!("") {
        let matches = occurrences(&line.ident.0);
        if matches.is_empty() {
            return LineResolution::Auto;
        }
        let index = if line.line_num == 0 {
            matches[0]
        } else if line.line_num > 0 {
            matches
                .get(line.line_num as usize - 1)
                .copied()
                .unwrap_or(*matches.last().unwrap())
        } else {
            let from_end = matches.len() as i64 + line.line_num as i64;
            matches
                .get(from_end.max(0) as usize)
                .copied()
                .unwrap_or(matches[0])
        };
        return LineResolution::Index(index);
    }
    if line.line_num == 0 {
        return LineResolution::Auto;
    }
    // Line numbers are 1-based and may be negative, counting back from the
    // end. Lines past the explicit track list would create implicit tracks;
    // clamp them to the last explicit line.
    let index = if line.line_num > 0 {
        line.line_num as i64 - 1
    } else {
        track_count as i64 + 1 + line.line_num as i64
    };
    LineResolution::Index((index.max(0) as usize).min(track_count))
}

/// Turn the resolved placement lines into the slice of tracks the item
/// covers: a start index and a number of tracks. A span on either side wins;
/// two definite lines give their distance; anything else covers one track.
fn span_and_offset(
    start: LineResolution,
    end: LineResolution,
    track_count: usize,
) -> (usize, usize) {
    let span = match (&start, &end) {
        (LineResolution::Span(span), _) | (_, LineResolution::Span(span)) => *span,
        (LineResolution::Index(start), LineResolution::Index(end)) => {
            end.saturating_sub(*start).max(1)
        },
        _ => 1,
    };
    let start_index = match (&start, &end) {
        (LineResolution::Index(start), _) => (*start).min(track_count.saturating_sub(1)),
        // A span or `auto` start with a definite end ends at that line.
        (_, LineResolution::Index(end)) => end.saturating_sub(span),
        _ => 0,
    };
    let count = span.min(track_count - start_index).max(1);
    (start_index, count)
}

/// The subgrid's own `subgrid <line-name-list>` names, as `count + 1` name
/// groups for its lines. Returns `None` when the list repeats names or
/// carries no plain groups, in which case the sliced parent names stand.
fn subgrid_line_names(
    names: &LineNameList<Integer>,
    count: usize,
) -> Option<Vec<OwnedSlice<CustomIdent>>> {
    if names.line_names.is_empty() {
        return None;
    }
    let mut groups = Vec::with_capacity(count + 1);
    for value in names.line_names.iter() {
        match value {
            LineNameListValue::LineNames(set) => groups.push(set.clone()),
            LineNameListValue::Repeat(_) => return None,
        }
    }
    groups.resize(count + 1, OwnedSlice::from(Vec::new()));
    groups.truncate(count + 1);
    Some(groups)
}

fn unnamed_line_names(count: usize) -> OwnedSlice<OwnedSlice<CustomIdent>> {
    (0..count)
        .map(|_| OwnedSlice::from(Vec::new()))
        .collect::<Vec<_>>()
        .into()
}

#[cfg(test)]
mod tests {
    use super::{LineResolution, span_and_offset};

    fn index(value: usize) -> LineResolution {
        LineResolution::Index(value)
    }

    #[test]
    fn definite_lines_give_their_distance() {
        assert_eq!(
            span_and_offset(index(0), index(2), 4),
            (0, 2),
            "a / c covers the first two tracks"
        );
        assert_eq!(
            span_and_offset(index(1), index(3), 4),
            (1, 2),
            "b / d covers the middle tracks"
        );
    }

    #[test]
    fn span_wins_over_the_other_line() {
        assert_eq!(
            span_and_offset(index(1), LineResolution::Span(3), 5),
            (1, 3),
            "b / span 3 covers three tracks from b"
        );
        assert_eq!(
            span_and_offset(LineResolution::Span(2), index(3), 5),
            (1, 2),
            "span 2 / d ends at d"
        );
    }

    #[test]
    fn auto_placement_covers_one_track() {
        assert_eq!(
            span_and_offset(LineResolution::Auto, LineResolution::Auto, 3),
            (0, 1),
            "auto / auto covers one track"
        );
        assert_eq!(
            span_and_offset(LineResolution::Auto, index(2), 3),
            (1, 1),
            "auto / c ends at c"
        );
    }

    #[test]
    fn ranges_clamp_to_the_parent_track_list() {
        assert_eq!(
            span_and_offset(index(1), LineResolution::Span(9), 3),
            (1, 2),
            "an overlong span covers the remaining tracks"
        );
        assert_eq!(
            span_and_offset(index(9), LineResolution::Span(1), 3),
            (2, 1),
            "a start line past the list clamps to the last track"
        );
    }
}
