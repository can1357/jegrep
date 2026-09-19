//! Shared-path exploration tracks. Files keep their places as their ranges settle.

use super::{Activity, Event, RangeState, SPINNER, clean, clip, paint, tail};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};
use unicode_width::UnicodeWidthStr;

const COMPLETION_HOLD: Duration = Duration::from_millis(1200);

#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct Counts {
    folders: usize,
    listed: usize,
    scanned: usize,
    names: usize,
    named: usize,
    naming: usize,
    files: usize,
    checked: usize,
    reading: usize,
    hits: usize,
    ranges: usize,
    settled: usize,
    running: usize,
    failed: usize,
}

impl Counts {
    fn replace(&mut self, old: Self, new: Self) {
        for (value, before, after) in [
            (&mut self.folders, old.folders, new.folders),
            (&mut self.listed, old.listed, new.listed),
            (&mut self.scanned, old.scanned, new.scanned),
            (&mut self.names, old.names, new.names),
            (&mut self.named, old.named, new.named),
            (&mut self.naming, old.naming, new.naming),
            (&mut self.files, old.files, new.files),
            (&mut self.checked, old.checked, new.checked),
            (&mut self.reading, old.reading, new.reading),
            (&mut self.hits, old.hits, new.hits),
            (&mut self.ranges, old.ranges, new.ranges),
            (&mut self.settled, old.settled, new.settled),
            (&mut self.running, old.running, new.running),
            (&mut self.failed, old.failed, new.failed),
        ] {
            *value = value.saturating_sub(before) + after;
        }
    }

    fn add(&mut self, other: Self) {
        self.replace(Self::default(), other);
    }
}

#[derive(Default)]
struct Folder {
    totals: Counts,
    direct: Counts,
    listed: bool,
}

struct Entry {
    activity: Activity,
    content: bool,
    name: Option<RangeState>,
    score: Option<f64>,
    detail: String,
    ranges: BTreeMap<(usize, usize), RangeState>,
    order: u64,
    changed: Instant,
    matched: Option<f64>,
}

impl Entry {
    fn busy(&self) -> bool {
        self.content
            && (self.activity == Activity::Reading
                || self
                    .ranges
                    .values()
                    .any(|s| matches!(s, RangeState::Active | RangeState::Queued)))
    }

    fn counts(&self) -> Counts {
        let mut counts = Counts {
            names: usize::from(self.name.is_some()),
            named: usize::from(self.name.is_some_and(settled)),
            naming: usize::from(self.name == Some(RangeState::Active)),
            files: usize::from(self.content),
            checked: usize::from(self.content && !self.busy()),
            reading: usize::from(self.busy()),
            hits: usize::from(self.matched.is_some()),
            ranges: self.ranges.len(),
            ..Counts::default()
        };
        for state in self.ranges.values() {
            counts.settled += usize::from(settled(*state));
            counts.running += usize::from(*state == RangeState::Active);
            counts.failed += usize::from(*state == RangeState::Failed);
        }
        counts
    }
}

const fn settled(state: RangeState) -> bool {
    matches!(
        state,
        RangeState::Scored(_) | RangeState::Pruned | RangeState::Failed
    )
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Phase {
    #[default]
    Explore,
    Scan,
    Names,
    Passages,
}

pub(super) struct Model {
    started: Instant,
    root: String,
    phase: Phase,
    status: String,
    round: Option<(u8, f64)>,
    names: Option<(usize, usize, usize)>,
    folders: BTreeMap<String, Folder>,
    // The initial workspace outline is useful before the first scan results.
    outline: Vec<String>,
    entries: BTreeMap<String, Entry>,
    serial: u64,
}

impl Default for Model {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            root: "workspace".into(),
            phase: Phase::Explore,
            status: "discovering folders".into(),
            round: None,
            names: None,
            folders: [(String::new(), Folder::default())].into(),
            outline: Vec::new(),
            entries: BTreeMap::new(),
            serial: 0,
        }
    }
}

impl Model {
    /// Inclusive ancestor rollup. Root files also have a separate direct lane.
    fn rollup(&mut self, path: &str, old: Counts, new: Counts) {
        let mut dir = path;
        loop {
            self.folders.get_mut(dir).unwrap().totals.replace(old, new);
            if dir.is_empty() {
                break;
            }
            dir = parent(dir.trim_end_matches('/'));
        }
    }

    fn folder(&mut self, path: &str) {
        // Prefixes always end at a slash, so UTF-8 path components stay intact.
        for (i, _) in path.match_indices('/') {
            let prefix = &path[..=i];
            if !self.folders.contains_key(prefix) {
                self.folders.insert(prefix.into(), Folder::default());
                self.rollup(
                    prefix,
                    Counts::default(),
                    Counts {
                        folders: 1,
                        ..Counts::default()
                    },
                );
                if parent(prefix.trim_end_matches('/')).is_empty() {
                    self.outline.push(prefix.into());
                }
            }
        }
    }

    fn root_files(&mut self) {
        if !self.outline.iter().any(String::is_empty) {
            self.outline.push(String::new());
        }
    }

    fn entry(&mut self, path: &str, update: impl FnOnce(&mut Entry)) {
        let dir = parent(path);
        self.folder(dir);
        if dir.is_empty() {
            self.root_files();
        }
        self.serial += 1;
        let entry = self.entries.entry(path.into()).or_insert_with(|| Entry {
            activity: Activity::Note,
            content: false,
            name: None,
            score: None,
            detail: String::new(),
            ranges: BTreeMap::new(),
            order: self.serial,
            changed: Instant::now(),
            matched: None,
        });
        let before = entry.counts();
        update(entry);
        entry.changed = Instant::now();
        let after = entry.counts();
        self.folders
            .get_mut(dir)
            .unwrap()
            .direct
            .replace(before, after);
        self.rollup(dir, before, after);
    }

    pub(super) fn update(&mut self, event: Event<'_>) {
        match event {
            Event::Workspace {
                name,
                folders,
                root_files,
            } => {
                self.root = clean(name);
                for path in folders {
                    self.folder(&clean(path));
                }
                if root_files {
                    self.root_files();
                }
            }
            Event::Scanned(path) => {
                let path = clean(path);
                let dir = parent(&path);
                self.folder(dir);
                if dir.is_empty() {
                    self.root_files();
                }
                let count = Counts {
                    scanned: 1,
                    ..Counts::default()
                };
                self.folders.get_mut(dir).unwrap().direct.add(count);
                self.rollup(dir, Counts::default(), count);
            }
            Event::Status(text) => {
                self.status = clean(text).trim().into();
                self.phase = match text {
                    "lexical scan" => Phase::Scan,
                    "filename ranking" => Phase::Names,
                    "passage scoring" => Phase::Passages,
                    _ => self.phase,
                };
            }
            Event::Round { number, threshold } => {
                self.round = Some((number, threshold));
            }
            Event::Names {
                done,
                total,
                active,
            } => {
                self.names = Some((done, total, active));
                if self.phase != Phase::Passages {
                    self.phase = Phase::Names;
                }
            }
            Event::Name { path, state } => {
                self.entry(&clean(path), |entry| entry.name = Some(state));
            }
            Event::Entry {
                activity: Activity::Folder | Activity::Collapsed,
                path,
                ..
            } => {
                // Some strategies use synthetic labels such as "(pool)" for batches.
                if !path.is_empty() && !path.ends_with('/') {
                    return;
                }
                let path = clean(path);
                self.folder(&path);
                let folder = self.folders.get_mut(&path).unwrap();
                if !folder.listed {
                    folder.listed = true;
                    self.rollup(
                        &path,
                        Counts::default(),
                        Counts {
                            listed: 1,
                            ..Counts::default()
                        },
                    );
                }
            }
            Event::Entry {
                activity: Activity::Note,
                detail,
                ..
            } => {
                self.status = clean(detail);
            }
            Event::Entry {
                activity,
                path,
                score,
                detail,
            } => {
                self.entry(&clean(path), |entry| {
                    if activity == Activity::Reading && entry.activity != Activity::Reading {
                        entry.ranges.clear();
                    }
                    entry.content |= matches!(activity, Activity::Reading | Activity::Hit);
                    entry.activity = activity;
                    entry.score = score;
                    if activity == Activity::Hit {
                        entry.matched = score;
                    } else if matches!(activity, Activity::Miss | Activity::Skipped) {
                        entry.matched = None;
                    }
                    entry.detail = clean(detail);
                    if matches!(activity, Activity::Hit | Activity::Miss | Activity::Skipped) {
                        for state in entry.ranges.values_mut() {
                            if !settled(*state) {
                                *state = RangeState::Pruned;
                            }
                        }
                    }
                });
            }
            Event::Range {
                path,
                start,
                end,
                state,
            } => {
                self.phase = Phase::Passages;
                self.entry(&clean(path), |entry| {
                    entry.content = true;
                    entry.ranges.insert((start, end), state);
                });
            }
        }
    }

    fn counts(&self, path: &str) -> Counts {
        let folder = &self.folders[path];
        if path.is_empty() {
            folder.direct
        } else {
            folder.totals
        }
    }

    fn phase_line(&self, spin: char, color: bool) -> String {
        let total = self.folders[""].totals;
        if self.phase == Phase::Explore {
            return paint(
                "36",
                &format!("{spin} exploring · {} folders visited", total.listed),
                color,
            );
        }
        let scan = format!(
            "{} scan {}",
            if self.phase == Phase::Scan {
                spin
            } else {
                '✓'
            },
            total.scanned
        );
        let names = self
            .names.map_or_else(|| "names".into(), |(done, total, _)| format!("names {done}/{total}"));
        let names = format!(
            "{} {names}",
            if self.phase == Phase::Names {
                spin
            } else if self.phase == Phase::Passages {
                '✓'
            } else {
                '○'
            }
        );
        let passage_label = match self.status.as_str() {
            "selecting passages from source sketches" => "selecting",
            "verifying selected source passages" => "verifying",
            _ => "passages",
        };
        let passages = if total.ranges > 0 {
            format!("{spin} {passage_label} {}/{}", total.settled, total.ranges)
        } else {
            format!(
                "{} passages",
                if self.phase == Phase::Passages {
                    spin
                } else {
                    '○'
                }
            )
        };
        format!(
            "{}  {}  {}  {}  {}",
            paint(
                if self.phase == Phase::Scan { "36" } else { "2" },
                &scan,
                color
            ),
            paint("2", "›", color),
            paint(
                if self.phase == Phase::Names {
                    "36"
                } else {
                    "2"
                },
                &names,
                color
            ),
            paint("2", "›", color),
            paint(
                if self.phase == Phase::Passages {
                    "36"
                } else {
                    "2"
                },
                &passages,
                color
            )
        )
    }

    /// Group work by its actual shared directory. Merge sibling groups only when
    /// the viewport is too small, preserving relative paths beneath that prefix.
    fn activity_groups(&self, limit: usize) -> Vec<(String, Vec<(&str, &Entry)>)> {
        let mut grouped: BTreeMap<String, Vec<(&str, &Entry)>> = BTreeMap::new();
        for (path, entry) in &self.entries {
            let relevant = if self.phase == Phase::Passages || self.folders[""].totals.files > 0 {
                entry.content
            } else {
                entry.name.is_some()
            };
            if relevant {
                grouped
                    .entry(parent(path).into())
                    .or_default()
                    .push((path, entry));
            }
        }
        while grouped.len() > limit {
            let keys: Vec<_> = grouped.keys().cloned().collect();
            let mut best = String::new();
            for pair in keys.windows(2) {
                let shared = common_directory(&pair[0], &pair[1]);
                if shared.len() > best.len() {
                    best = shared;
                }
            }
            if best.is_empty() {
                break;
            }
            let children: Vec<_> = grouped
                .keys()
                .filter(|p| p.starts_with(&best))
                .cloned()
                .collect();
            let mut entries = Vec::new();
            for child in children {
                entries.extend(grouped.remove(&child).unwrap());
            }
            grouped.insert(best, entries);
        }
        let mut groups: Vec<_> = grouped.into_iter().collect();
        for (_, entries) in &mut groups {
            entries.sort_by_key(|(_, e)| e.order);
        }
        groups.sort_by_key(|(_, entries)| entries.iter().map(|(_, e)| e.order).min().unwrap_or(0));
        groups
    }

    fn file_track(&self, entry: &Entry, cells: usize, color: bool, tick: usize) -> String {
        let states: Vec<_> = if entry.ranges.is_empty() {
            vec![if entry.busy() {
                RangeState::Active
            } else {
                entry.name.unwrap_or(RangeState::Pruned)
            }]
        } else {
            entry.ranges.values().copied().collect()
        };
        track(&states, cells, color, tick)
    }

    fn file_line(
        &self,
        prefix: &str,
        path: &str,
        entry: &Entry,
        width: usize,
        color: bool,
        tick: usize,
    ) -> String {
        let active: Vec<_> = entry
            .ranges
            .iter()
            .filter(|(_, s)| **s == RangeState::Active)
            .collect();
        let waiting = entry.busy()
            && !entry.ranges.is_empty()
            && active.is_empty()
            && entry.ranges.values().any(|s| *s == RangeState::Queued);
        let (glyph, code, detail) = if let Some(score) = entry.matched {
            let glyph = if entry.changed.elapsed() < COMPLETION_HOLD && tick % 4 < 2 {
                '✦'
            } else {
                '●'
            };
            (glyph, "1;32", format!("found {score:.2}"))
        } else if let Some(((start, end), _)) = active.first() {
            (
                SPINNER[tick % SPINNER.len()],
                "36",
                format!(
                    "L{start}–{end}{}",
                    if active.len() > 1 {
                        format!(" +{}", active.len() - 1)
                    } else {
                        String::new()
                    }
                ),
            )
        } else if waiting || (!entry.content && entry.name == Some(RangeState::Queued)) {
            ('○', "2", "queued".into())
        } else if entry.busy() || (!entry.content && entry.name == Some(RangeState::Active)) {
            (
                SPINNER[tick % SPINNER.len()],
                "36",
                if entry.content {
                    "reading".into()
                } else {
                    "by name".into()
                },
            )
        } else if entry.content {
            (
                '✓',
                "2",
                if entry.ranges.values().any(|s| *s == RangeState::Failed) {
                    "check failed".into()
                } else {
                    "checked".into()
                },
            )
        } else {
            (
                '·',
                "2",
                match entry.name {
                    Some(RangeState::Scored(p)) => format!("name {p:.2}"),
                    Some(RangeState::Failed) => "name failed".into(),
                    _ => "visited".into(),
                },
            )
        };
        let cells = if width >= 94 {
            24
        } else if width >= 70 {
            16
        } else {
            10
        };
        let name_width = width.saturating_sub(cells + 24).clamp(6, 30);
        let name = tail(path.strip_prefix(prefix).unwrap_or(path), name_width);
        let padding = " ".repeat(name_width.saturating_sub(name.width()));
        format!(
            "   {} {}{}  {}  {}",
            paint(code, &glyph.to_string(), color),
            paint(
                if entry.matched.is_some() {
                    "1"
                } else if entry.busy() {
                    "0"
                } else {
                    "2"
                },
                &name,
                color
            ),
            padding,
            self.file_track(entry, cells, color, tick),
            paint(code, &detail, color)
        )
    }

    pub(super) fn lines(
        &mut self,
        columns: usize,
        rows: usize,
        color: bool,
        tick: usize,
    ) -> Vec<String> {
        let width = columns.saturating_sub(1);
        let height = rows.saturating_sub(1).min(30);
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let totals = self.folders[""].totals;
        let mut lines = vec![
            format!(
                " {}  {}",
                paint("1", &self.root, color),
                paint(
                    "2",
                    &format!("{:.1}s", self.started.elapsed().as_secs_f64()),
                    color
                )
            ),
            format!(" {}", self.phase_line(SPINNER[tick % SPINNER.len()], color)),
            String::new(),
        ];
        let group_limit = if height >= 20 {
            4
        } else if height >= 12 {
            2
        } else {
            1
        };
        let groups = self.activity_groups(group_limit);
        let visible = groups.len().min(group_limit);
        // Share file rows fairly, returning spare rows from small folders to
        // larger branches instead of leaving empty space in the viewport.
        let mut budget =
            height.saturating_sub(4 + visible * 2 + usize::from(groups.len() > visible));
        let mut slots = vec![0; visible];
        while budget > 0 {
            let mut allocated = false;
            for (index, (_, entries)) in groups.iter().take(visible).enumerate() {
                if budget > 0 && slots[index] < entries.len() {
                    slots[index] += 1;
                    budget -= 1;
                    allocated = true;
                }
            }
            if !allocated {
                break;
            }
        }
        for (index, (prefix, entries)) in groups.iter().take(visible).enumerate() {
            let slots = slots[index];
            if slots == 0 {
                break;
            }
            let shown = if entries.len() > slots {
                slots.saturating_sub(1)
            } else {
                entries.len()
            };
            lines.push(format!(
                " {}",
                folder_title(if prefix.is_empty() { "./" } else { prefix }, color)
            ));
            for &(path, entry) in entries.iter().take(shown) {
                lines.push(self.file_line(prefix, path, entry, width, color, tick));
            }
            if entries.len() > shown && slots > 0 {
                let remaining = &entries[shown..];
                let cells = if width >= 94 {
                    24
                } else if width >= 70 {
                    16
                } else {
                    10
                };
                let name_width = width.saturating_sub(cells + 24).clamp(6, 30);
                let label = tail(&format!("+{} files", remaining.len()), name_width);
                let states: Vec<_> = remaining.iter().map(|(_, e)| file_state(e)).collect();
                let active = remaining
                    .iter()
                    .filter(|(_, e)| work_priority(e) == 0)
                    .count();
                let found = remaining
                    .iter()
                    .filter(|(_, e)| e.matched.is_some())
                    .count();
                let detail = if active > 0 {
                    format!("{active} active")
                } else if found > 0 {
                    format!("{found} found")
                } else if remaining.iter().all(|(_, e)| work_priority(e) == 2) {
                    "checked".into()
                } else {
                    "waiting".into()
                };
                lines.push(format!(
                    "   {} {}{}  {}  {}",
                    paint("2", "·", color),
                    paint("2", &label, color),
                    " ".repeat(name_width.saturating_sub(label.width())),
                    track(&states, cells, color, tick),
                    paint("2", &detail, color)
                ));
            }
            lines.push(String::new());
        }
        if groups.len() > visible {
            lines.push(format!(
                " {}",
                paint(
                    "2",
                    &format!("+{} other folders being explored", groups.len() - visible),
                    color
                )
            ));
        }
        if groups.is_empty() {
            // During the local scan, retain the workspace outline and let each
            // branch leave a trail of scanned files instead of flashing filenames.
            let shown = self.outline.len().min(height.saturating_sub(6));
            for path in self.outline.iter().take(shown) {
                let c = self.counts(path);
                let name_width = width.saturating_sub(40).max(8);
                let label = tail(if path.is_empty() { "./" } else { path }, name_width);
                let count = c.scanned;
                let cells = 16;
                // Scan totals are unknown. These dots are accumulated reads,
                // with a moving pulse, rather than a completion percentage.
                let trail: String = (0..cells)
                    .map(|i| {
                        let visited = i < count.min(cells);
                        paint(
                            if visited { "34" } else { "2" },
                            if visited { "•" } else { "·" },
                            color,
                        )
                    })
                    .collect();
                lines.push(format!(
                    "   {} {}{}  {}  {}",
                    paint("36", &SPINNER[tick % SPINNER.len()].to_string(), color),
                    paint("34", &label, color),
                    " ".repeat(name_width.saturating_sub(label.width())),
                    trail,
                    paint("2", &format!("{count} scanned"), color)
                ));
            }
            if shown == 0 {
                lines.push(format!(
                    " {} scanning workspace…",
                    paint("36", &SPINNER[tick % SPINNER.len()].to_string(), color)
                ));
            }
            lines.push(String::new());
        }
        lines.push(format!(
            " {}  {}",
            paint(
                "2",
                &format!(
                    "{} scanned · {} folders · {} checking",
                    totals.scanned, totals.folders, totals.reading
                ),
                color
            ),
            paint(
                if totals.hits > 0 { "32" } else { "2" },
                &format!(
                    "{} found{}",
                    totals.hits,
                    if totals.failed > 0 {
                        format!(" · {} failed", totals.failed)
                    } else {
                        String::new()
                    }
                ),
                color
            )
        ));
        lines.truncate(height);
        lines.into_iter().map(|line| clip(&line, width)).collect()
    }
}

fn parent(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..=i])
}

fn common_directory(a: &str, b: &str) -> String {
    let mut shared = String::new();
    for (x, y) in a.split('/').zip(b.split('/')) {
        if x.is_empty() || x != y {
            break;
        }
        shared.push_str(x);
        shared.push('/');
    }
    shared
}

fn work_priority(entry: &Entry) -> u8 {
    if entry.ranges.values().any(|s| *s == RangeState::Active)
        || (entry.content && entry.busy() && entry.ranges.is_empty())
        || (!entry.content && entry.name == Some(RangeState::Active))
    {
        0
    } else if entry.busy() || (!entry.content && entry.name == Some(RangeState::Queued)) {
        1
    } else {
        2
    }
}

fn folder_title(path: &str, color: bool) -> String {
    let trimmed = path.trim_end_matches('/');
    let prefix = parent(trimmed);
    format!(
        "{}{}",
        paint("2;34", prefix, color),
        paint("1;34", &path[prefix.len()..], color)
    )
}

fn file_state(entry: &Entry) -> RangeState {
    if let Some(score) = entry.matched {
        RangeState::Scored(score)
    } else if work_priority(entry) == 0 {
        RangeState::Active
    } else if work_priority(entry) == 1 {
        RangeState::Queued
    } else {
        entry.name.unwrap_or(RangeState::Pruned)
    }
}

fn track(states: &[RangeState], cells: usize, color: bool, tick: usize) -> String {
    const HEAT: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if states.is_empty() {
        return paint("2", &"·".repeat(cells), color);
    }
    (0..cells)
        .map(|i| {
            let start = i * states.len() / cells;
            let end = ((i + 1) * states.len() / cells).max(start + 1);
            let state = states[start..end.min(states.len())]
                .iter()
                .copied()
                .max_by_key(|s| match s {
                    RangeState::Active => 500,
                    RangeState::Failed => 400,
                    RangeState::Queued => 300,
                    RangeState::Scored(p) => 100 + (p * 100.0) as u32,
                    RangeState::Pruned => 0,
                })
                .unwrap();
            let (glyph, code) = match state {
                RangeState::Active => (SPINNER[(tick + i) % SPINNER.len()], "36"),
                RangeState::Queued => ('░', "2"),
                RangeState::Pruned => ('·', "2"),
                RangeState::Failed => ('!', "31"),
                RangeState::Scored(p) => (
                    HEAT[(p.clamp(0.0, 1.0) * 7.0).round() as usize],
                    if p >= 0.7 {
                        "32"
                    } else if p >= 0.4 {
                        "33"
                    } else {
                        "2"
                    },
                ),
            };
            paint(code, &glyph.to_string(), color)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Model {
        let mut m = Model::default();
        m.update(Event::Workspace {
            name: "workspace",
            folders: &["src/".into(), "tests/".into(), "docs/".into()],
            root_files: true,
        });
        m.update(Event::Status("passage scoring"));
        m.update(Event::Entry {
            activity: Activity::Folder,
            path: "src/config/",
            score: None,
            detail: "",
        });
        m.update(Event::Names {
            done: 64,
            total: 128,
            active: 2,
        });
        reading(&mut m, "src/config/解決器.rs");
        for (start, state) in [
            (1, RangeState::Scored(0.1)),
            (21, RangeState::Active),
            (41, RangeState::Queued),
            (61, RangeState::Pruned),
            (81, RangeState::Failed),
        ] {
            m.update(Event::Range {
                path: "src/config/解決器.rs",
                start,
                end: start + 19,
                state,
            });
        }
        m
    }

    fn reading(m: &mut Model, path: &str) {
        m.update(Event::Entry {
            activity: Activity::Reading,
            path,
            score: Some(0.8),
            detail: "reading content",
        });
    }

    #[test]
    fn file_tracks_keep_their_rows_as_parallel_checks_finish() {
        let mut m = fixture();
        for n in 0..10 {
            reading(&mut m, &format!("src/config/file{n}.rs"));
        }
        let before = m.lines(120, 30, false, 0);
        let positions = |lines: &[String]| {
            (0..10)
                .map(|n| {
                    lines
                        .iter()
                        .position(|l| l.contains(&format!("file{n}.rs")))
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        let order = positions(&before);
        for n in [8, 1, 7, 0, 4, 2] {
            m.update(Event::Entry {
                activity: Activity::Hit,
                path: &format!("src/config/file{n}.rs"),
                score: Some(0.9),
                detail: "",
            });
        }
        assert_eq!(positions(&m.lines(120, 30, false, 7)), order);
        assert_eq!(m.folders["src/"].totals.hits, 6);
        assert_eq!(m.folders["src/"].totals.checked, 6);
    }

    #[test]
    fn small_folders_return_spare_rows_to_larger_branches() {
        let mut m = Model::default();
        for n in 0..6 {
            reading(&mut m, &format!("src/hashline/file{n}.rs"));
        }
        for path in ["packages/a.ts", "tests/a.rs", "docs/a.md"] {
            reading(&mut m, path);
        }
        let lines = m.lines(96, 24, false, 0).join("\n");
        for n in 0..6 {
            assert!(lines.contains(&format!("file{n}.rs")), "{lines}");
        }
        assert!(lines.contains("a.ts") && lines.contains("a.md"));
        assert!(lines.contains("9 checking"));
    }

    #[test]
    fn content_work_animates_after_filename_scoring_and_settles_after_failure() {
        let mut m = Model::default();
        let path = "src/parser.rs";
        m.update(Event::Name {
            path,
            state: RangeState::Scored(0.9),
        });
        reading(&mut m, path);
        let first = m.file_track(&m.entries[path], 16, false, 0);
        let next = m.file_track(&m.entries[path], 16, false, 1);
        assert_ne!(first, next);
        m.update(Event::Range {
            path,
            start: 1,
            end: 80,
            state: RangeState::Failed,
        });
        m.update(Event::Entry {
            activity: Activity::Skipped,
            path,
            score: None,
            detail: "failed",
        });
        let lines = m.lines(96, 24, false, 0).join("\n");
        assert!(lines.contains("check failed"));
        assert!(lines.contains("0 checking") && lines.contains("1 failed"));
    }

    #[test]
    fn sibling_activity_folds_to_a_real_common_directory_with_relative_names() {
        let mut m = Model::default();
        reading(&mut m, "src/modes/hashline/parser.rs");
        reading(&mut m, "src/modes/hashline/patcher.rs");
        reading(&mut m, "src/modes/diff/apply.rs");
        let groups = m.activity_groups(1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, "src/modes/");
        let rows: Vec<_> = groups[0]
            .1
            .iter()
            .map(|(p, e)| m.file_line(&groups[0].0, p, e, 160, false, 0))
            .collect();
        assert!(rows[0].contains("hashline/parser.rs"));
        assert!(rows[1].contains("hashline/patcher.rs"));
        assert!(rows[2].contains("diff/apply.rs"));
    }

    #[test]
    fn discoveries_stay_inline_without_a_border_or_separate_result_list() {
        let mut m = fixture();
        for (path, score) in [("src/b.rs", 0.97), ("lib/a.rs", 0.59), ("src/a.rs", 0.97)] {
            m.update(Event::Entry {
                activity: Activity::Hit,
                path,
                score: Some(score),
                detail: "",
            });
        }
        reading(&mut m, "src/b.rs");
        let lines = m.lines(120, 30, false, 0);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("b.rs") && l.contains("found 0.97"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("a.rs") && l.contains("found 0.59"))
        );
        assert_eq!(m.folders[""].totals.hits, 3);
        assert!(!lines.iter().any(|line| line.contains("matches ·")
            || line.contains('╭')
            || line.contains('│')
            || line.contains('└')));
    }

    #[test]
    fn repeated_events_and_reopened_files_do_not_double_count() {
        let mut m = fixture();
        let path = "src/config/解決器.rs";
        for _ in 0..3 {
            m.update(Event::Name {
                path,
                state: RangeState::Scored(0.8),
            });
            m.update(Event::Entry {
                activity: Activity::Hit,
                path,
                score: Some(0.9),
                detail: "",
            });
        }
        let counts = m.folders["src/"].totals;
        assert_eq!(
            (
                counts.hits,
                counts.files,
                counts.checked,
                counts.names,
                counts.named
            ),
            (1, 1, 1, 1, 1)
        );
        reading(&mut m, path);
        let counts = m.folders["src/"].totals;
        assert_eq!(
            (counts.hits, counts.files, counts.checked, counts.ranges),
            (1, 1, 0, 0)
        );
    }

    #[test]
    fn names_scan_and_root_files_roll_up_without_sibling_prefix_collisions() {
        let mut m = Model::default();
        for path in ["src/a.rs", "src-more/b.rs", "root.rs"] {
            m.update(Event::Scanned(path));
            m.update(Event::Name {
                path,
                state: RangeState::Queued,
            });
            m.update(Event::Name {
                path,
                state: RangeState::Active,
            });
        }
        assert_eq!(m.folders[""].totals.scanned, 3);
        assert_eq!(m.counts("src/").scanned, 1);
        assert_eq!(m.counts("").names, 1);
        m.update(Event::Name {
            path: "src/a.rs",
            state: RangeState::Failed,
        });
        assert_eq!(m.counts("src/").named, 1);
        assert_eq!(m.counts("src/").naming, 0);
        assert_eq!(m.counts("src-more/").naming, 1);
    }

    #[test]
    fn frames_fit_tiny_resized_and_unicode_terminals() {
        let mut m = fixture();
        for n in 0..20 {
            m.folder(&format!("extra{n}/"));
        }
        for columns in [1, 2, 12, 35, 60, 80, 140] {
            for rows in [1, 2, 3, 6, 10, 24] {
                for color in [false, true] {
                    let frame = m.lines(columns, rows, color, 0);
                    assert!(frame.len() < rows);
                    for line in frame {
                        assert!(clean(&line).width() < columns, "{columns}: {line:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn range_states_and_original_coordinates_remain_visible_without_row_rotation() {
        let mut m = fixture();
        let first = m.lines(100, 24, false, 0).join("\n");
        assert!(
            first.contains("解決器.rs") && first.contains("L21–40"),
            "{first}"
        );
        assert!(first.contains("1 failed"));
        assert!(first.contains("names 64/128"));
        assert!(first.contains("src/config/"));
        assert!(!first.contains('\x1b'));
        assert_ne!(first, m.lines(100, 24, false, 1).join("\n"));
    }
}
