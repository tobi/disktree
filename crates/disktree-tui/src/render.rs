//! Compact terminal projection of disktree's mosaic and cleanup flow.

use std::path::Path;
use std::time::Duration;

use disktree_core::classify::Category;
use disktree_core::insights::Finding;
use disktree_core::removal::RemovalMode;
use disktree_core::size::{human_bytes, human_bytes_short, human_count};
use disktree_core::tree::{Metric, path_of};
use disktree_core::treemap::TileKind;
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Screen, ViewMode};
use crate::theme::{
    ACCENT, AGE_LABELS, BG, MUTED, PANEL, Palette, RED, TEXT, age_bucket,
    base_age_color, base_category_color as color, shade,
};

pub fn draw(frame: &mut Frame<'_>, app: &App, palette: &Palette) {
    app.begin_animation_frame();
    draw_base(frame, app);
    palette.apply(frame.buffer_mut());
}

fn draw_base(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    if area.width < 45 || area.height < 13 {
        centered(frame, area, "disktree needs at least 45 × 13 cells");
        return;
    }
    // The list names each category, so its header does not need a legend.
    let head_height = app.header_height(area.width);
    let head = Rect::new(area.x, area.y, area.width, head_height);
    let body = app.body_area(area);
    let foot = Rect::new(area.x, area.bottom() - 2, area.width, 2);
    header(frame, app, head);
    match app.screen {
        Screen::Explore => explore(frame, app, body),
        Screen::Review => review(frame, app, body),
        Screen::Confirm => confirm(frame, app, body),
        Screen::Running | Screen::Done => results(frame, app, body),
        Screen::Help => help(frame, body),
        Screen::Insights => insights(frame, app, body),
        Screen::SavePrompt => save_prompt(frame, app, body),
        Screen::Volumes => volume_picker(frame, app, body),
    }
    footer(frame, app, foot);
}

fn header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let path = app.current_path().unwrap_or_else(|| app.root.clone());
    let path = readable_path(&path.display().to_string());
    let depth_width: u16 = if app.screen == Screen::Explore { 16 } else { 0 };
    let wide_controls = app.screen == Screen::Explore && area.width >= 120;
    let top_controls = wide_controls.then(|| header_controls(app, false));
    let top_controls_width = top_controls.as_ref().map_or(0, |controls| {
        controls
            .iter()
            .map(|span| span.content.chars().count())
            .sum()
    });
    let reserved = usize::from(depth_width)
        + top_controls_width
        + if wide_controls { 3 } else { 1 };
    let title_width = area
        .width
        .saturating_sub(u16::try_from(reserved).unwrap_or(u16::MAX));
    let path_width = usize::from(title_width).saturating_sub(16);
    // A dark rule splits each colored cell into two panes. The blank cell
    // makes the vertical gutter visible even when a font joins box edges.
    let pane = Style::default().fg(BG).bg(ACCENT);
    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled("━", pane),
        Span::raw(" "),
        Span::styled("━", pane),
        Span::styled(
            " disktree ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", focused_marquee(app, &path, path_width)),
            Style::default().fg(TEXT),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).style(Style::default().bg(BG)),
        Rect::new(area.x, area.y, title_width, 1),
    );
    if let Some(controls) = top_controls {
        frame.render_widget(
            Paragraph::new(Line::from(controls)).style(Style::default().bg(BG)),
            Rect::new(
                area.x + title_width + 1,
                area.y,
                u16::try_from(top_controls_width).unwrap_or(u16::MAX),
                1,
            ),
        );
    }
    if depth_width > 0 && area.width >= depth_width {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("[", Style::default().fg(ACCENT).bg(PANEL)),
                Span::raw(" "),
                Span::styled("]", Style::default().fg(ACCENT).bg(PANEL)),
                Span::raw(" "),
                Span::styled("-", Style::default().fg(ACCENT).bg(PANEL)),
                Span::raw("/"),
                Span::styled("+", Style::default().fg(ACCENT).bg(PANEL)),
                Span::styled(" Depth ", Style::default().fg(MUTED)),
                Span::styled(
                    app.view_depth.to_string(),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]))
            .style(Style::default().bg(BG)),
            Rect::new(area.right() - depth_width, area.y, depth_width, 1),
        );
    }
    let scan = if app.scan.is_some() {
        format!(
            "scanning {} files · {} dirs",
            human_count(app.progress.files),
            human_count(app.progress.dirs)
        )
    } else if let Some(node) = app.tree.as_ref() {
        format!(
            "{} · {} files · {} dirs",
            human_bytes(node.bytes),
            human_count(node.files),
            human_count(u64::from(node.dirs))
        )
    } else {
        app.scan_error
            .clone()
            .unwrap_or_else(|| "waiting".to_string())
    };
    let compact = area.width < 60;
    let stats = if compact && app.scan.is_none() {
        app.tree
            .as_ref()
            .map_or_else(|| scan.clone(), |node| human_bytes(node.bytes))
    } else {
        scan
    };
    if wide_controls {
        put(
            frame,
            Rect::new(area.x, area.y + 1, area.width, 1),
            format!(" {}", end_clip(&stats, usize::from(area.width - 1))),
            Style::default().fg(MUTED).bg(BG),
        );
    } else {
        let controls = if app.screen == Screen::Explore {
            header_controls(app, compact)
        } else {
            let measure = if app.options.apparent_size {
                "apparent"
            } else {
                "disk"
            };
            let hidden = if app.options.include_hidden {
                "hidden"
            } else {
                "visible"
            };
            vec![Span::styled(
                format!(
                    "{} · {hidden} · {measure}",
                    app.mode.label().to_ascii_lowercase()
                ),
                Style::default().fg(MUTED),
            )]
        };
        let controls_width: usize = controls
            .iter()
            .map(|span| span.content.chars().count())
            .sum();
        let stats_width = usize::from(area.width)
            .saturating_sub(controls_width + if compact { 4 } else { 6 });
        let separator = if compact { " │ " } else { "  │  " };
        let mut line = vec![
            Span::styled(
                format!(" {}", end_clip(&stats, stats_width)),
                Style::default().fg(MUTED),
            ),
            Span::styled(separator, Style::default().fg(MUTED)),
        ];
        line.extend(controls);
        frame.render_widget(
            Paragraph::new(Line::from(line)).style(Style::default().bg(BG)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
    if area.height < 3 {
        return;
    }
    if app.mode == ViewMode::Age {
        age_legend(frame, area);
        return;
    }
    // Keep every category visible as a complete label. Intermediate terminals
    // have room for the mosaic, but not the desktop-length legend.
    let short = [
        "Code", "Agent", "Tools", "Sync", "Git", "Media", "Docs", "Cache",
    ];
    let (reclaim, separator) = if area.width >= 105 {
        ("  / Reclaimable", "  ▪ ")
    } else if area.width >= 77 {
        (" / Reclaim", "  ▪ ")
    } else if area.width >= 61 {
        (" / Reclaim", " ▪")
    } else {
        (" / Rclm", " ▪")
    };
    let mut legend = vec![Span::styled(reclaim, Style::default().fg(MUTED))];
    legend.extend(Category::LEGEND.iter().enumerate().map(
        |(index, category)| {
            let label = if area.width >= 105 {
                category.label()
            } else {
                short[index]
            };
            Span::styled(
                format!("{separator}{label}"),
                Style::default().fg(color(*category)),
            )
        },
    ));
    let legend = Line::from(legend);
    frame.render_widget(
        Paragraph::new(legend).style(Style::default().bg(BG)),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
}

fn header_controls(app: &App, compact: bool) -> Vec<Span<'static>> {
    let measure = if app.options.apparent_size {
        "Apparent"
    } else {
        "Disk"
    };
    let hidden = if app.options.include_hidden {
        "Hidden"
    } else {
        "Visible"
    };
    let gap = if compact { " " } else { " · " };
    let label = Style::default().fg(TEXT);
    let key = Style::default().fg(ACCENT).bg(PANEL);
    vec![
        Span::styled("t", key),
        Span::styled(format!(" {}", app.mode.label()), label),
        Span::raw(gap),
        Span::styled("i", key),
        Span::styled(format!(" {hidden}"), label),
        Span::raw(gap),
        Span::styled("d", key),
        Span::styled(format!(" {measure}"), label),
    ]
}

fn age_legend(frame: &mut Frame<'_>, area: Rect) {
    let compact = area.width < 75;
    let names = if compact {
        ["Week", "Month", "6 months", "Year", "Older"]
    } else {
        AGE_LABELS
    };
    let mut legend = vec![Span::styled(" / Age", Style::default().fg(MUTED))];
    legend.extend(names.into_iter().enumerate().map(|(index, name)| {
        Span::styled(
            format!("  ▪ {name}"),
            Style::default().fg(base_age_color(index)),
        )
    }));
    frame.render_widget(
        Paragraph::new(Line::from(legend)).style(Style::default().bg(BG)),
        Rect::new(area.x, area.y + 2, area.width, 1),
    );
}

fn explore(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.tree.is_none() {
        if app.scan.is_some() || app.progress.cancelled {
            scanning_panel(frame, app, area);
        } else {
            centered(frame, area, "No scan results. Press r to retry.");
        }
        return;
    }
    let (map, detail_area, side) = App::split_explore(area);
    map_or_list(frame, app, map);
    if side {
        detail(frame, app, detail_area);
    } else {
        compact_detail(frame, app, detail_area);
    }
}

fn scanning_panel(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(PANEL)),
        area,
    );
    let width = area.width.saturating_sub(4).min(72);
    let x = area.x + (area.width - width) / 2;
    let top = area.y + area.height.saturating_sub(7) / 2;
    let cancelled = app.progress.cancelled && app.scan.is_none();
    let verb = if cancelled {
        "Stopped reading "
    } else {
        "Reading "
    };
    let path = readable_path(&app.root.display().to_string());
    let path = focused_marquee(
        app,
        &path,
        usize::from(width).saturating_sub(verb.len()),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(verb, Style::default().fg(ACCENT).bg(PANEL)),
            Span::styled(path, Style::default().fg(TEXT).bg(PANEL)),
        ])),
        Rect::new(x, top, width, 1),
    );

    let files = human_count(app.progress.files);
    let dirs = human_count(app.progress.dirs);
    let bytes = human_bytes(app.progress.bytes);
    let errors = human_count(app.progress.errors);
    let compact = area.width < 65;
    let lines = if compact {
        vec![
            format!("{files} files · {dirs} directories"),
            format!("{bytes} measured · {errors} unreadable"),
        ]
    } else {
        vec![format!(
            "{files} files   {dirs} directories   {bytes} measured   {errors} unreadable"
        )]
    };
    for (index, line) in lines.into_iter().enumerate() {
        put(
            frame,
            Rect::new(x, top + 2 + index as u16, width, 1),
            end_clip(&line, usize::from(width)),
            Style::default().fg(TEXT).bg(PANEL),
        );
    }

    if !cancelled {
        let meter_width = width.min(52);
        let meter = Rect::new(x, top + 4, meter_width, 1);
        fill(frame.buffer_mut(), meter, Style::default().bg(BG));
        let measured = (f32::from(meter_width)
            * scan_activity(app.progress.files))
        .round() as u16;
        fill(
            frame.buffer_mut(),
            Rect::new(x, meter.y, measured.min(meter_width), 1),
            Style::default().bg(ACCENT),
        );
    }
    let explanation = if cancelled {
        "Nothing is shown from a scan that did not finish."
    } else if compact {
        "Map and marks appear when the scan lands."
    } else {
        "The map, marks and disk meter appear when the scan lands."
    };
    put(
        frame,
        Rect::new(x, top + 5, width, 1),
        end_clip(explanation, usize::from(width)),
        Style::default().fg(MUTED).bg(PANEL),
    );
    let (key, action) = if cancelled {
        ("r", " scan again")
    } else {
        ("Esc", " cancel scan")
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(key, Style::default().fg(ACCENT).bg(PANEL)),
            Span::styled(action, Style::default().fg(MUTED).bg(PANEL)),
        ])),
        Rect::new(x, top + 6, width, 1),
    );
}

/// The walk has no known total. Like the GUI, this is a moving activity
/// meter that approaches the end without claiming a completion percentage.
fn scan_activity(files: u64) -> f32 {
    if files == 0 {
        0.06
    } else {
        let scaled = files as f32 / (files as f32 + 20_000.0);
        (0.1 + scaled * 0.85).min(0.99)
    }
}

fn map_or_list(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.is_list(area) {
        ranked_list(frame, app, area);
    } else {
        treemap(frame, app, area);
    }
}

fn treemap(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // Six visible characters plus an ellipsis make a clipped name readable.
    // Below that, the selected tile's detail line carries its full label.
    const MIN_TILE_LABEL_COLUMNS: u16 = 7;
    let Some(root) = app.tree.as_ref() else {
        return;
    };
    let tiles = app.tiles(area);
    let buffer = frame.buffer_mut();
    let mut selected_rect = None;
    for tile in tiles.iter() {
        let rect = App::tile_rect(tile, area);
        if rect.width == 0 || rect.height == 0 {
            continue;
        }
        let (label, value, category, marked, reclaimable) = match &tile.kind {
            TileKind::Node { crumbs } => {
                let Some(item) = root.resolve(crumbs) else {
                    continue;
                };
                (
                    clean(&item.name),
                    item.value(app.options.metric),
                    item.category,
                    !app.marks.is_empty()
                        && app.is_marked(&path_of(&app.root, root, crumbs)),
                    item.reclaim.is_some(),
                )
            }
            TileKind::Others { count, .. } => {
                (format!("+{count} more"), 0, Category::Other, false, false)
            }
        };
        let selected = matches!(&tile.kind,
            TileKind::Node { crumbs } if app.selected.as_ref() == Some(crumbs));
        let base = if app.mode == ViewMode::Age {
            match &tile.kind {
                TileKind::Node { crumbs } => root
                    .resolve(crumbs)
                    .and_then(|item| age_bucket(item.modified, app.scanned_at))
                    .map_or_else(|| color(category), base_age_color),
                TileKind::Others { .. } => color(category),
            }
        } else {
            color(category)
        };
        let bg = if marked {
            Color::Rgb(92, 43, 54)
        } else {
            shade(base, 0.05_f32.mul_add(tile.depth as f32, 0.32))
        };
        fill(buffer, rect, Style::default().bg(bg).fg(TEXT));
        if reclaimable && !marked {
            hatch(buffer, rect, shade(base, 0.68));
        }
        if selected {
            selected_rect = Some(rect);
        } else if tile.depth == 0 {
            top_border(buffer, rect, base);
        }
        let left = rect.x + u16::from(selected);
        let top = rect.y + u16::from(selected && rect.height > 2);
        let inner_width = rect.width.saturating_sub(1 + u16::from(selected));
        if inner_width >= MIN_TILE_LABEL_COLUMNS && top < rect.bottom() {
            let label = if selected {
                focused_marquee(app, &label, usize::from(inner_width))
            } else {
                marquee(app, &label, usize::from(inner_width))
            };
            buffer.set_stringn(
                left,
                top,
                label,
                usize::from(inner_width),
                Style::default().fg(TEXT).bg(bg).add_modifier(
                    if tile.depth == 0 {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    },
                ),
            );
            if value > 0 {
                let bottom = tile.header.map_or_else(
                    || rect.bottom(),
                    |header| area.y + (header.bottom() / 2.0).ceil() as u16,
                );
                let bottom = bottom
                    .min(rect.bottom().saturating_sub(u16::from(selected)));
                let size = match app.options.metric {
                    Metric::Bytes => human_bytes_short(value),
                    Metric::Files => human_count(value),
                };
                if let Some((value, unit)) = fit_size_label(
                    &size,
                    usize::from(inner_width),
                    usize::from(bottom.saturating_sub(top + 1)),
                ) {
                    let style = Style::default().fg(MUTED).bg(bg);
                    buffer.set_string(left, top + 1, value, style);
                    if let Some(unit) = unit {
                        buffer.set_string(left, top + 2, unit, style);
                    }
                }
            }
        }
    }
    // Paint the selected edge last: rounded child tiles can reach its cells.
    if let Some(rect) = selected_rect {
        outline(buffer, rect, ACCENT);
    }
}

/// Prefer a compact unit before using a second terminal row.
fn fit_size_label(
    size: &str,
    width: usize,
    rows: usize,
) -> Option<(String, Option<String>)> {
    if rows == 0 {
        return None;
    }
    if size.len() <= width {
        return Some((size.to_string(), None));
    }
    let split = size.find(|ch: char| ch.is_ascii_alphabetic())?;
    let (value, unit) = size.split_at(split);
    let short = format!("{value}{}", unit.chars().next()?);
    if short.len() <= width {
        return Some((short, None));
    }
    (rows >= 2 && value.len() <= width && unit.len() <= width)
        .then(|| (value.to_string(), Some(unit.to_string())))
}

fn ranked_list(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(node) = app.current_node() else {
        return;
    };
    let visible = app.visible_children();
    let total = visible.len();
    let capacity = usize::from(area.height.saturating_sub(1));
    let start = app.list_scroll.min(total.saturating_sub(capacity));
    let end = (start + capacity).min(total);
    let range = if total == 0 {
        "0/0".to_string()
    } else {
        format!("{}-{end}/{total}", start + 1)
    };
    let hint = if area.width < 65 {
        format!(
            " CONTENTS {range} · {} files {} dirs",
            human_count(node.files),
            human_count(u64::from(node.dirs))
        )
    } else {
        format!(" CONTENTS {range}  arrows select · Enter open · Backspace up")
    };
    let mut lines = vec![Line::styled(hint, Style::default().fg(MUTED))];
    for position in start..end {
        let index = visible.get(position).expect("visible position in range");
        let child = &node.children[index];
        let mut crumbs = app.current.clone();
        crumbs.push(index);
        let selected = app.selected.as_ref() == Some(&crumbs);
        let marked = !app.marks.is_empty()
            && app.tree.as_ref().is_some_and(|root| {
                app.is_marked(&path_of(&app.root, root, &crumbs))
            });
        let sigil = if marked {
            "◆"
        } else if selected {
            "▸"
        } else {
            " "
        };
        let size = match app.options.metric {
            Metric::Bytes => human_bytes_short(child.bytes),
            Metric::Files => human_count(child.files),
        };
        let category_color = if app.mode == ViewMode::Age {
            age_bucket(child.modified, app.scanned_at)
                .map_or_else(|| color(child.category), base_age_color)
        } else {
            color(child.category)
        };
        let style =
            Style::default().fg(if selected { ACCENT } else { category_color });
        let label = if app.mode == ViewMode::Age {
            let age = age_bucket(child.modified, app.scanned_at).map_or(
                "Unknown",
                |bucket| match bucket {
                    0 => "Week",
                    1 => "Month",
                    2 => "6 months",
                    3 => "Year",
                    _ => "Older",
                },
            );
            format!(" [{age}]")
        } else {
            format!(" [{}]", child.category.label())
        };
        let name_width =
            usize::from(area.width).saturating_sub(14 + label.len());
        lines.push(Line::from(vec![
            Span::styled(format!(" {sigil} "), style),
            Span::styled(format!("{size:>8}  "), Style::default().fg(MUTED)),
            Span::styled(
                if selected {
                    focused_marquee(app, &clean(&child.name), name_width)
                } else {
                    marquee(app, &clean(&child.name), name_width)
                },
                style,
            ),
            Span::styled(label, Style::default().fg(category_color)),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(PANEL)),
        area,
    );
}

fn detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(PANEL)),
        area,
    );
    let Some(node) = app.selected_node().or_else(|| app.current_node()) else {
        return;
    };
    let path = app.selected_path().unwrap_or_else(|| app.root.clone());
    let inner = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    if inner.height < 3 {
        return;
    }
    let selected = Line::from(vec![Span::styled(
        "SELECTION\n",
        Style::default().fg(MUTED),
    )]);
    frame.render_widget(
        Paragraph::new(selected),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    put(
        frame,
        Rect::new(inner.x, inner.y + 2, inner.width, 1),
        focused_marquee(app, &clean(&node.name), usize::from(inner.width)),
        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
    );
    put(
        frame,
        Rect::new(inner.x, inner.y + 3, inner.width, 1),
        focused_marquee(
            app,
            &path_tail(&path, &app.root),
            usize::from(inner.width),
        ),
        Style::default().fg(MUTED),
    );
    put(
        frame,
        Rect::new(inner.x, inner.y + 5, inner.width, 1),
        match app.options.metric {
            Metric::Bytes => human_bytes(node.bytes),
            Metric::Files => format!("{} files", human_count(node.files)),
        },
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    if inner.height >= 11 {
        let value = node.value(app.options.metric);
        let parent = app
            .current_node()
            .map_or(value, |parent| parent.value(app.options.metric));
        let share = if parent == 0 {
            0.0
        } else {
            value as f64 / parent as f64
        };
        frame.render_widget(
            Gauge::default()
                .ratio(share.clamp(0.0, 1.0))
                .gauge_style(Style::default().fg(ACCENT).bg(BG)),
            Rect::new(inner.x, inner.y + 6, inner.width, 1),
        );
        put(
            frame,
            Rect::new(inner.x, inner.y + 8, inner.width, 1),
            match app.options.metric {
                Metric::Bytes => format!(
                    "{} files  ·  {} dirs",
                    human_count(node.files),
                    human_count(u64::from(node.dirs))
                ),
                Metric::Files => format!(
                    "{}  ·  {} dirs",
                    human_bytes(node.bytes),
                    human_count(u64::from(node.dirs))
                ),
            },
            Style::default().fg(MUTED),
        );
        put(
            frame,
            Rect::new(inner.x, inner.y + 9, inner.width, 1),
            if app.mode == ViewMode::Age {
                age_bucket(node.modified, app.scanned_at)
                    .map_or("Age unknown", |bucket| AGE_LABELS[bucket])
            } else {
                node.category.label()
            },
            Style::default().fg(if app.mode == ViewMode::Age {
                age_bucket(node.modified, app.scanned_at)
                    .map_or_else(|| color(node.category), base_age_color)
            } else {
                color(node.category)
            }),
        );
    }
    if inner.height >= 20 && !app.insights.is_empty() {
        let total: u64 = app.insights.iter().map(|item| item.bytes).sum();
        put(
            frame,
            Rect::new(inner.x, inner.y + 12, inner.width, 1),
            "WORTH A LOOK",
            Style::default().fg(MUTED),
        );
        let total = human_bytes(total);
        put(
            frame,
            Rect::new(
                inner.right().saturating_sub(total.len() as u16),
                inner.y + 12,
                total.len() as u16,
                1,
            ),
            total,
            Style::default().fg(ACCENT),
        );
        for (index, candidate) in app.insights.iter().take(3).enumerate() {
            let y = inner.y + 14 + index as u16 * 2;
            if y + 1 >= inner.bottom().saturating_sub(5) {
                break;
            }
            if let Some(root) = app.tree.as_ref() {
                let path = path_of(&app.root, root, &candidate.crumbs);
                let size = human_bytes_short(candidate.bytes);
                let path_width = usize::from(inner.width)
                    .saturating_sub(text_width(&size) + 4);
                let name =
                    marquee(app, &path_tail(&path, &app.root), path_width);
                let category = root
                    .resolve(&candidate.crumbs)
                    .map_or(Category::Other, |node| node.category);
                put(
                    frame,
                    Rect::new(inner.x, y, 1, 1),
                    " ",
                    Style::default().bg(color(category)),
                );
                put(
                    frame,
                    Rect::new(inner.x + 2, y, path_width as u16, 1),
                    name,
                    Style::default().fg(TEXT),
                );
                put(
                    frame,
                    Rect::new(
                        inner.right().saturating_sub(size.len() as u16),
                        y,
                        size.len() as u16,
                        1,
                    ),
                    size,
                    Style::default().fg(TEXT),
                );
                put(
                    frame,
                    Rect::new(
                        inner.x + 2,
                        y + 1,
                        inner.width.saturating_sub(2),
                        1,
                    ),
                    end_clip(
                        &finding_reason(&candidate.finding),
                        usize::from(inner.width.saturating_sub(2)),
                    ),
                    Style::default().fg(MUTED),
                );
            }
        }
    }
    if inner.height >= 6 {
        let y = inner.bottom() - 5;
        disk_header(frame, app, Rect::new(inner.x, y, inner.width, 1));
        if let Some(space) = app.space {
            put(
                frame,
                Rect::new(inner.x, y + 1, inner.width, 1),
                format!("{} free", human_bytes(space.available)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            );
            frame.render_widget(
                Gauge::default()
                    .ratio(f64::from(space.used_fraction()))
                    .gauge_style(Style::default().fg(MUTED).bg(BG)),
                Rect::new(inner.x, y + 2, inner.width, 1),
            );
            put(
                frame,
                Rect::new(inner.x, y + 3, inner.width, 1),
                format!(
                    "{} used / {} total",
                    human_bytes(space.used()),
                    human_bytes(space.total)
                ),
                Style::default().fg(MUTED),
            );
        }
    }
}

fn compact_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Block::default().style(Style::default().bg(PANEL)),
        area,
    );
    if let Some(node) = app.selected_node() {
        let suffix = match app.options.metric {
            Metric::Bytes => format!(
                "  ·  {}  ·  {} files",
                human_bytes(node.bytes),
                human_count(node.files)
            ),
            Metric::Files => format!(
                "  ·  {} files  ·  {}",
                human_count(node.files),
                human_bytes(node.bytes)
            ),
        };
        let label_width = usize::from(area.width.saturating_sub(2))
            .saturating_sub(2 + text_width(&suffix));
        let label = focused_marquee(app, &clean(&node.name), label_width);
        put(
            frame,
            Rect::new(area.x + 1, area.y, area.width.saturating_sub(2), 1),
            format!("▸ {label}{suffix}"),
            Style::default().fg(TEXT),
        );
    }
    let line =
        Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
    if area.height >= 5 || app.space.is_none() {
        disk_header(frame, app, line);
    }
    if let Some(space) = app.space {
        if area.height >= 5 {
            put(
                frame,
                Rect::new(line.x, line.y + 1, line.width, 1),
                format!("{} free", human_bytes(space.available)),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            );
        } else {
            put(
                frame,
                line,
                compact_disk_line(
                    app,
                    space.available,
                    usize::from(line.width),
                ),
                Style::default().fg(ACCENT),
            );
        }
        if area.height >= 3 {
            let used = space.used_fraction();
            frame.render_widget(
                Gauge::default()
                    .ratio(f64::from(used))
                    .label(format!("{:.0}% used", used * 100.0))
                    .gauge_style(Style::default().fg(MUTED).bg(BG)),
                Rect::new(
                    area.x + 1,
                    area.y + if area.height >= 5 { 3 } else { 2 },
                    area.width.saturating_sub(2),
                    1,
                ),
            );
        }
        if area.height >= 5 {
            put(
                frame,
                Rect::new(
                    area.x + 1,
                    area.y + 4,
                    area.width.saturating_sub(2),
                    1,
                ),
                format!(
                    "{} used / {} total",
                    human_bytes(space.used()),
                    human_bytes(space.total)
                ),
                Style::default().fg(MUTED),
            );
        }
    } else if area.height >= 5 {
        put(
            frame,
            Rect::new(line.x, line.y + 1, line.width, 1),
            "Free space is unavailable",
            Style::default().fg(MUTED),
        );
    }
}

fn disk_path(app: &App) -> String {
    app.device.as_ref().map_or_else(
        || readable_path(&app.root.display().to_string()),
        |path| readable_path(path),
    )
}

fn compact_disk_line(app: &App, available: u64, width: usize) -> String {
    let suffix = format!(" · {} free", human_bytes(available));
    let path_width = width.saturating_sub(5 + text_width(&suffix));
    format!("DISK {}{suffix}", marquee(app, &disk_path(app), path_width))
}

fn end_clip(value: &str, width: usize) -> String {
    if text_width(value) <= width {
        return value.to_string();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", prefix_cells(value, width - 1))
}

fn text_width(value: &str) -> usize {
    if value.is_ascii() {
        value.len()
    } else {
        UnicodeWidthStr::width(value)
    }
}

fn prefix_cells(value: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for part in value.graphemes(true) {
        let cells = text_width(part);
        if used + cells > width {
            break;
        }
        result.push_str(part);
        used += cells;
    }
    result
}

fn marquee(app: &App, value: &str, width: usize) -> String {
    marquee_at(app, value, width, app.marquee_elapsed())
}

fn focused_marquee(app: &App, value: &str, width: usize) -> String {
    marquee_at(app, value, width, app.focused_marquee_elapsed())
}

fn marquee_at(
    app: &App,
    value: &str,
    width: usize,
    elapsed: Duration,
) -> String {
    let (label, next) = sliding_window(value, width, elapsed);
    if let Some(delay) = next {
        app.schedule_animation(delay);
    }
    label
}

/// Reveal the whole value in steps, with a pause at each end. The ellipsis
/// stays at the right edge while more text remains beyond the window.
fn sliding_window(
    value: &str,
    width: usize,
    elapsed: Duration,
) -> (String, Option<Duration>) {
    if width == 0 {
        return (String::new(), None);
    }
    let total = text_width(value);
    if total <= width {
        return (value.to_string(), None);
    }
    if width == 1 {
        return ("…".to_string(), None);
    }
    if value.is_ascii() {
        let (offset, next) = marquee_position(total - width, elapsed);
        let remaining = total - offset;
        let hidden_right = remaining > width;
        let content_width = width - usize::from(hidden_right);
        let mut visible = value[offset..offset + content_width].to_string();
        if hidden_right {
            visible.push('…');
        }
        return (visible, Some(next));
    }
    let parts: Vec<(&str, usize)> = value
        .graphemes(true)
        .map(|part| (part, text_width(part)))
        .collect();
    let mut remaining = total;
    let mut end_start = 0;
    while remaining > width && end_start < parts.len() {
        remaining = remaining.saturating_sub(parts[end_start].1);
        end_start += 1;
    }
    let (offset, next) = marquee_position(end_start, elapsed);
    let remaining = parts[offset..]
        .iter()
        .map(|(_, cells)| cells)
        .sum::<usize>();
    let hidden_right = remaining > width;
    let content_width = width - usize::from(hidden_right);
    let mut visible = String::new();
    let mut used = 0;
    for &(part, cells) in &parts[offset..] {
        if used + cells > content_width {
            break;
        }
        visible.push_str(part);
        used += cells;
    }
    if hidden_right {
        visible.push('…');
    }
    (visible, Some(next))
}

/// Return the visible grapheme offset and the next time its window changes.
fn marquee_position(steps: usize, elapsed: Duration) -> (usize, Duration) {
    const PAUSE_MS: u128 = 800;
    const STEP_MS: u128 = 200;
    // A long path should reach its basename in seconds, even when it is
    // hundreds of cells wider than the panel.
    const MAX_SLIDE_MS: u128 = 5000;
    let step_count = u128::try_from(steps).unwrap_or(u128::MAX);
    let slide_ms = step_count.saturating_mul(STEP_MS).min(MAX_SLIDE_MS);
    let cycle = PAUSE_MS.saturating_add(slide_ms).saturating_add(PAUSE_MS);
    let phase = elapsed.as_millis() % cycle;
    let (offset, next_ms) = if phase < PAUSE_MS {
        (0, PAUSE_MS - phase)
    } else if phase < PAUSE_MS + slide_ms {
        let progress = phase - PAUSE_MS;
        let moved = progress.saturating_mul(step_count) / slide_ms + 1;
        let next_progress = moved.saturating_mul(slide_ms).div_ceil(step_count);
        (
            usize::try_from(moved).unwrap_or(steps).min(steps),
            next_progress.saturating_sub(progress).max(1),
        )
    } else {
        (steps, cycle - phase)
    };
    (
        offset,
        Duration::from_millis(u64::try_from(next_ms).unwrap_or(u64::MAX)),
    )
}

fn disk_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let disk = marquee(
        app,
        &disk_path(app),
        usize::from(area.width.saturating_sub(5)),
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("DISK ", Style::default().fg(ACCENT)),
            Span::styled(disk, Style::default().fg(MUTED)),
        ])),
        area,
    );
}

fn review(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let plan = app.plan();
    let mode = if app.removal_mode == RemovalMode::Trash {
        format!(
            "{} ({})",
            app.removal_mode.label(),
            app.trash_backend.label()
        )
    } else {
        app.removal_mode.label().to_string()
    };
    let mut lines = vec![
        Line::styled(
            "  REVIEW MARKED PATHS",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "  {} targets · {} projected · {}",
                plan.targets.len(),
                human_bytes(plan.bytes()),
                mode
            ),
            Style::default().fg(TEXT),
        ),
        Line::raw(""),
    ];
    for target in plan
        .targets
        .iter()
        .skip(app.scroll)
        .take(usize::from(area.height))
    {
        let prefix = format!("  ◆ {:>9}  ", human_bytes_short(target.bytes));
        lines.push(Line::from(vec![
            Span::styled(prefix.clone(), Style::default().fg(ACCENT)),
            Span::styled(
                marquee(
                    app,
                    &readable_path(&target.path.display().to_string()),
                    usize::from(area.width).saturating_sub(text_width(&prefix)),
                ),
                Style::default().fg(TEXT),
            ),
        ]));
    }
    for blocked in plan
        .blocked
        .iter()
        .take(usize::from(area.height).saturating_sub(lines.len()))
    {
        let suffix = format!(" — {}", clean(&blocked.reason));
        let path = marquee(
            app,
            &readable_path(&blocked.path.display().to_string()),
            usize::from(area.width).saturating_sub(5 + text_width(&suffix)),
        );
        lines.push(Line::styled(
            format!("  ! {path}{suffix}"),
            Style::default().fg(RED),
        ));
    }
    if let Some(space) = app.space {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!(
                "  DISK  {} free → {} projected",
                human_bytes(space.available),
                human_bytes(space.after_removing(plan.bytes()).available)
            ),
            Style::default().fg(ACCENT),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  a  hand to agent (stdout)    s  save agent prompt    Enter  proceed",
        Style::default().fg(MUTED),
    ));
    lines.push(Line::styled(
        "  m  trash    p  permanent    !  clear marks    Esc  explore",
        Style::default().fg(MUTED),
    ));
    panel(frame, area, lines);
}

fn confirm(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lines = vec![
        Line::styled(
            "  CONFIRM PERMANENT DELETION",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(
            format!(
                "  {} paths · {} projected",
                app.plan().targets.len(),
                human_bytes(app.marked_bytes())
            ),
            Style::default().fg(TEXT),
        ),
        Line::raw(""),
        Line::styled(
            "  This cannot be undone. Press y to delete; Esc cancels.",
            Style::default().fg(RED),
        ),
    ];
    panel(frame, area, lines);
}

fn results(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let done = app.screen == Screen::Done;
    let heading = if done && app.run_cancelled {
        "  REMOVAL STOPPED"
    } else if done {
        "  REMOVAL COMPLETE"
    } else {
        "  REMOVING MARKED PATHS"
    };
    let mut lines = vec![
        Line::styled(
            heading,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!(
                "  {} of {} removed · {} failed",
                app.run_summary.removed,
                app.run_summary.total,
                app.run_summary.failed
            ),
            Style::default().fg(TEXT),
        ),
        Line::raw(""),
    ];
    for (path, outcome) in app
        .run_log
        .iter()
        .rev()
        .take(usize::from(area.height.saturating_sub(7)))
    {
        let (mark, style) = if outcome.is_ok() {
            ("✓", Style::default().fg(ACCENT))
        } else {
            ("!", Style::default().fg(RED))
        };
        let path = marquee(
            app,
            &readable_path(&path.display().to_string()),
            usize::from(area.width).saturating_sub(4),
        );
        lines.push(Line::styled(format!("  {mark} {path}"), style));
    }
    if done {
        lines.push(Line::raw(""));
        let gain = app.measured_gain.map_or_else(
            || "unavailable".to_string(),
            |bytes| {
                if bytes >= 0 {
                    human_bytes(bytes as u64)
                } else {
                    format!("-{}", human_bytes(bytes.unsigned_abs() as u64))
                }
            },
        );
        lines.push(Line::styled(
            format!("  Measured change in free space: {gain}"),
            Style::default().fg(ACCENT),
        ));
        lines.push(Line::styled(
            "  Enter or Esc returns to the map",
            Style::default().fg(MUTED),
        ));
    } else {
        lines.push(Line::styled(
            "  Esc stops after the current path",
            Style::default().fg(MUTED),
        ));
    }
    panel(frame, area, lines);
}

fn insights(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let heading = "  WORTH A LOOK";
    let total: u64 = app.insights.iter().map(|item| item.bytes).sum();
    let total = if total > 0 {
        human_bytes(total)
    } else {
        String::new()
    };
    let gap = usize::from(area.width)
        .saturating_sub(text_width(heading) + text_width(&total));
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                heading,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(total, Style::default().fg(ACCENT)),
        ]),
        Line::raw(""),
    ];
    if app.insights.is_empty() {
        lines.push(Line::styled(
            "  No notable reclaimable paths in this scan.",
            Style::default().fg(MUTED),
        ));
    }
    let rows = usize::from(area.height.saturating_sub(6) / 2).max(1);
    let start = app.insight_index.saturating_sub(rows / 2);
    for (index, item) in app.insights.iter().enumerate().skip(start).take(rows)
    {
        let Some(root) = app.tree.as_ref() else { break };
        let path = path_of(&app.root, root, &item.crumbs);
        let selected = index == app.insight_index;
        let name_color = if selected { ACCENT } else { TEXT };
        let category = root
            .resolve(&item.crumbs)
            .map_or(Category::Other, |node| node.category);
        let size = human_bytes_short(item.bytes);
        let path_width =
            usize::from(area.width).saturating_sub(text_width(&size) + 8);
        let path = path_tail(&path, &app.root);
        let path = if selected {
            focused_marquee(app, &path, path_width)
        } else {
            marquee(app, &path, path_width)
        };
        let padding = " ".repeat(path_width.saturating_sub(text_width(&path)));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if selected { "▸" } else { " " },
                Style::default().fg(ACCENT),
            ),
            Span::raw(" "),
            Span::styled("▪", Style::default().fg(color(category))),
            Span::raw(" "),
            Span::styled(
                format!("{path}{padding}"),
                Style::default().fg(name_color),
            ),
            Span::raw("  "),
            Span::styled(size, Style::default().fg(TEXT)),
        ]));
        lines.push(Line::styled(
            format!(
                "      {}",
                end_clip(
                    &finding_reason(&item.finding),
                    usize::from(area.width).saturating_sub(6),
                )
            ),
            Style::default().fg(MUTED),
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "  j/k select · Enter reveal · Esc close",
        Style::default().fg(MUTED),
    ));
    panel(frame, area, lines);
}

fn finding_reason(finding: &Finding) -> String {
    match finding {
        Finding::Reclaimable(reason) => reason.label().to_string(),
        Finding::Worktrees { count, oldest_days } => {
            format!("{count} worktrees · oldest {oldest_days} days")
        }
        Finding::StaleExperiments { count } => {
            format!("{count} stale experiments")
        }
    }
}

fn volume_picker(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut lines = vec![
        Line::styled(
            "  SCAN A VOLUME",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            "  j/k select · Enter scan · Esc cancel",
            Style::default().fg(MUTED),
        ),
        Line::raw(""),
    ];
    if app.volumes_loading {
        lines.push(Line::styled(
            "  Looking for mounted volumes…",
            Style::default().fg(MUTED),
        ));
    } else if app.volumes.is_empty() {
        lines.push(Line::styled(
            "  No other readable volumes found.",
            Style::default().fg(MUTED),
        ));
    } else {
        let rows = usize::from(area.height.saturating_sub(5)).max(1);
        let start = app.volume_highlight.saturating_sub(rows / 2);
        for (index, volume) in
            app.volumes.iter().enumerate().skip(start).take(rows)
        {
            let selected = index == app.volume_highlight;
            let free = volume.space.map_or_else(
                || "free unknown".to_string(),
                |space| format!("{} free", human_bytes(space.available)),
            );
            let path = readable_path(&volume.point.display().to_string());
            let max_path = usize::from(area.width)
                .saturating_sub(free.chars().count() + 7);
            lines.push(Line::styled(
                format!(
                    "  {} {}  {free}",
                    if selected { '>' } else { ' ' },
                    if selected {
                        focused_marquee(app, &path, max_path)
                    } else {
                        marquee(app, &path, max_path)
                    }
                ),
                Style::default().fg(if selected { ACCENT } else { TEXT }),
            ));
        }
    }
    panel(frame, area, lines);
}

fn save_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) {
    panel(
        frame,
        area,
        vec![
            Line::styled("  SAVE AGENT PROMPT", Style::default().fg(ACCENT)),
            Line::raw(""),
            Line::styled(
                format!(
                    "  File: {}_",
                    if app.save_path.is_empty() {
                        "disktree-agent-prompt.txt".to_string()
                    } else {
                        clean(&app.save_path)
                    }
                ),
                Style::default().fg(TEXT),
            ),
            Line::raw(""),
            Line::styled(
                "  Enter creates the file · Esc cancels",
                Style::default().fg(MUTED),
            ),
        ],
    );
}

fn help(frame: &mut Frame<'_>, area: Rect) {
    if area.height < 18 {
        panel(
            frame,
            area,
            vec![
                Line::styled("  DISKTREE KEYS", Style::default().fg(ACCENT)),
                guide_line(&[("arrows", "move"), ("Enter", "open")], BG),
                guide_line(
                    &[("Backspace", "up"), ("Tab", "next"), ("Space", "mark")],
                    BG,
                ),
                guide_line(&[("c", "review"), ("/", "filter")], BG),
                guide_line(
                    &[("m", "trash"), ("p", "delete"), ("a", "agent")],
                    BG,
                ),
                guide_line(
                    &[("v", "list"), ("V", "volumes"), ("g", "disk")],
                    BG,
                ),
                guide_line(&[("[ ] -/+", "depth"), ("w", "insights")], BG),
                guide_line(
                    &[
                        ("t", "size/files/age"),
                        ("i", "hidden"),
                        ("d", "disk/app"),
                    ],
                    BG,
                ),
                guide_line(
                    &[("r", "rescan"), ("q", "quit"), ("Esc", "close")],
                    BG,
                ),
            ],
        );
        return;
    }
    panel(
        frame,
        area,
        vec![
            Line::styled(
                "  DISKTREE KEYS",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
            help_row("arrows / hjkl", "Move among visible tiles"),
            help_row("Tab / Shift-Tab", "Next / previous visible tile"),
            help_row("Enter", "Open a directory"),
            help_row("Backspace", "Go to parent"),
            help_row("Space", "Mark or unmark selection"),
            help_row("c", "Review marked paths"),
            help_row("w", "Worth a look"),
            help_row("/", "Filter by name"),
            help_row("v", "Toggle map/scrolling list"),
            help_row("V", "Choose a volume or drive"),
            help_row("g", "Scan this disk from its root"),
            help_row("[ / ] or - / +", "Show fewer / more tile levels"),
            help_row("t", "Size / Files / Age"),
            help_row("i / d", "Hidden entries / disk or apparent size"),
            help_row("r", "Rescan"),
            help_row("q / Ctrl-C", "Quit"),
            Line::raw(""),
            Line::styled(
                "  Press any key to close",
                Style::default().fg(MUTED),
            ),
        ],
    );
}

fn help_row(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(" {key} "),
            Style::default()
                .fg(ACCENT)
                .bg(BG)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{}{label}",
                " ".repeat(16_usize.saturating_sub(key.chars().count() + 2))
            ),
            Style::default().fg(TEXT),
        ),
    ])
}

fn guide_line(hints: &[(&str, &str)], key_bg: Color) -> Line<'static> {
    guide_line_with_gap(hints, key_bg, 2)
}

fn guide_line_with_gap(
    hints: &[(&str, &str)],
    key_bg: Color,
    gap: usize,
) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in hints {
        spans.push(Span::styled(
            format!("{key} "),
            Style::default()
                .fg(ACCENT)
                .bg(key_bg)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{label}{}", " ".repeat(gap)),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

/// Leave out entire key hints when the terminal is too narrow for them.
fn guide_line_fit(
    hints: &[(&str, &str)],
    width: usize,
    key_bg: Color,
) -> Line<'static> {
    let mut used = 1;
    let fit: Vec<_> = hints
        .iter()
        .copied()
        .take_while(|(key, label)| {
            let cells = text_width(key) + text_width(label) + 2;
            if used + cells > width {
                return false;
            }
            used += cells;
            true
        })
        .collect();
    guide_line_with_gap(&fit, key_bg, 1)
}

fn footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    let line = if app.search_open {
        Line::raw(format!(" /{}_", clean(&app.search)))
    } else if let Some(notice) = &app.notice {
        Line::raw(format!(" {}", clean(notice)))
    } else {
        let short = area.width < 65;
        let hints: &[(&str, &str)] = match app.screen {
            Screen::Explore if short => &[
                ("?", "help"),
                ("Space", "mark"),
                ("Enter", "open"),
                ("hjkl", "move"),
            ],
            Screen::Explore if area.width >= 100 => &[
                ("?", "help"),
                ("Space", "mark"),
                ("Enter", "open"),
                ("Backspace", "up"),
                ("c", "review"),
                ("hjkl", "move"),
                ("/", "filter"),
                ("[ ]", "depth"),
                ("V", "volumes"),
                ("r", "rescan"),
                ("w", "look"),
                ("v", "list"),
            ],
            Screen::Explore => &[
                ("?", "help"),
                ("Space", "mark"),
                ("Enter", "open"),
                ("Bksp", "up"),
                ("c", "review"),
                ("hjkl", "move"),
                ("/", "filter"),
                ("w", "look"),
                ("v", "list"),
            ],
            Screen::Review if short => &[
                ("a", "agent"),
                ("m", "trash"),
                ("p", "delete"),
                ("Enter", "go"),
            ],
            Screen::Review => &[
                ("a", "agent prompt"),
                ("s", "save prompt"),
                ("Enter", "proceed"),
                ("Esc", "explore"),
            ],
            Screen::Confirm => &[("y", "confirm"), ("Esc", "cancel")],
            Screen::Running => &[("Esc", "stop after current path")],
            Screen::Done => &[("Enter", "return to map")],
            Screen::Help | Screen::Insights => &[("Esc", "close")],
            Screen::Volumes => {
                &[("j/k", "select"), ("Enter", "scan"), ("Esc", "cancel")]
            }
            Screen::SavePrompt => {
                &[("Enter", "create file"), ("Esc", "cancel")]
            }
        };
        guide_line_fit(hints, usize::from(area.width), PANEL)
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().fg(MUTED).bg(BG)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let scan = scan_footer(app, area.width);
    let scan_width = u16::try_from(scan.chars().count())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let left_width = area.width.saturating_sub(scan_width.saturating_add(1));
    let free = app.space.map_or_else(
        || "free unknown".to_string(),
        |space| format!("{} free", human_bytes(space.available)),
    );
    let mut status = format!(" {free}");
    if !app.marks.is_empty() {
        let projection = if area.width < 65 { "est" } else { "projected" };
        let full = format!(
            "{status} · {} marked · {} {projection}",
            app.marks.len(),
            human_bytes(app.marked_bytes())
        );
        let short = format!("{status} · {} marked", app.marks.len());
        if full.chars().count() <= usize::from(left_width) {
            status = full;
        } else if short.chars().count() <= usize::from(left_width) {
            status = short;
        }
    }
    put(
        frame,
        Rect::new(area.x, area.y + 1, left_width, 1),
        status,
        Style::default().fg(ACCENT),
    );
    put(
        frame,
        Rect::new(area.right() - scan_width, area.y + 1, scan_width, 1),
        scan,
        Style::default().fg(MUTED),
    );
}

fn scan_footer(app: &App, width: u16) -> String {
    let count =
        human_count(app.progress.files.saturating_add(app.progress.dirs));
    let short = width < 65;
    if app.scan.is_some() {
        return if short {
            format!("scanning · {count}")
        } else {
            format!(
                "scanning · {count} entries · {}",
                human_bytes(app.progress.bytes)
            )
        };
    }
    if app.progress.cancelled {
        return format!("scan stopped · {count}");
    }
    if app.scan_error.is_some() {
        return "scan failed".to_string();
    }
    if let Some(elapsed) = app.scan_elapsed {
        return if short {
            format!("scan {count} · {:.1}s", elapsed.as_secs_f32())
        } else {
            format!("scan {count} entries · {:.1}s", elapsed.as_secs_f32())
        };
    }
    format!("scan {count} entries")
}

fn panel(frame: &mut Frame<'_>, area: Rect, lines: Vec<Line<'_>>) {
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().fg(TEXT).bg(PANEL))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(MUTED)),
            ),
        area,
    );
}

fn centered(frame: &mut Frame<'_>, area: Rect, message: &str) {
    let y = area.y + area.height / 2;
    put(
        frame,
        Rect::new(area.x + 2, y, area.width.saturating_sub(4), 1),
        message,
        Style::default().fg(MUTED),
    );
}

fn put(
    frame: &mut Frame<'_>,
    area: Rect,
    value: impl Into<String>,
    style: Style,
) {
    if area.width > 0 && area.height > 0 {
        frame.render_widget(Paragraph::new(value.into()).style(style), area);
    }
}

fn fill(buffer: &mut Buffer, rect: Rect, style: Style) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            buffer[(x, y)].set_symbol(" ").set_style(style);
        }
    }
}

fn hatch(buffer: &mut Buffer, rect: Rect, color: Color) {
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            if (x + y * 2) % 5 == 0 {
                buffer[(x, y)].set_symbol("/").set_fg(color);
            }
        }
    }
}

fn top_border(buffer: &mut Buffer, rect: Rect, color: Color) {
    for x in rect.x..rect.right() {
        buffer[(x, rect.y)].set_symbol("─").set_fg(color);
    }
}

fn outline(buffer: &mut Buffer, rect: Rect, color: Color) {
    for x in rect.x..rect.right() {
        buffer[(x, rect.y)].set_symbol("─").set_fg(color);
        if rect.height > 1 {
            buffer[(x, rect.bottom() - 1)].set_symbol("─").set_fg(color);
        }
    }
    for y in rect.y..rect.bottom() {
        buffer[(rect.x, y)].set_symbol("│").set_fg(color);
        if rect.width > 1 {
            buffer[(rect.right() - 1, y)].set_symbol("│").set_fg(color);
        }
    }
}

fn path_tail(path: &Path, root: &Path) -> String {
    path.strip_prefix(root).map_or_else(
        |_| readable_path(&path.display().to_string()),
        |tail| {
            format!(
                ".{}{}",
                std::path::MAIN_SEPARATOR,
                clean(&tail.display().to_string())
            )
        },
    )
}

fn readable_path(path: &str) -> String {
    let path = clean(path);
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(&path).to_string()
}

fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::insights::Candidate;
    use disktree_core::removal::Target;
    use disktree_core::scan::{ScanOptions, scan};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    #[test]
    fn scan_panel_uses_the_gui_reading_and_activity_grammar() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        app.progress = disktree_core::scan::ScanSnapshot {
            files: 1200,
            dirs: 40,
            bytes: 50 * 1024 * 1024,
            errors: 2,
            ..Default::default()
        };
        for (width, height) in [(45, 13), (80, 24), (120, 35)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app, &Palette::dark()))
                .expect("draw");
            let output: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            for phrase in [
                "━ ━ disktree",
                "Reading",
                "files",
                "directories",
                "50 MiB measured",
                "2 unreadable",
                "Esc cancel scan",
            ] {
                assert!(output.contains(phrase), "width {width}: {phrase}");
            }
            assert!(!output.contains('%'), "activity is not a percent");
            let body = app.body_area(Rect::new(0, 0, width, height));
            let meter_y = body.y + body.height.saturating_sub(7) / 2 + 4;
            let meter_x = (width - width.saturating_sub(4).min(72)) / 2;
            let fill = terminal
                .backend()
                .buffer()
                .cell((meter_x, meter_y))
                .expect("meter cell");
            assert_eq!(fill.bg, ACCENT);
        }
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.progress.cancelled = true;
        let mut terminal =
            Terminal::new(TestBackend::new(45, 13)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("cancelled draw");
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(output.contains("Stopped reading"));
        assert!(output.contains("r scan again"));
    }

    #[test]
    fn wide_controls_share_the_title_row_with_depth() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree =
            Some(scan(temp.path(), ScanOptions::default()).expect("scan"));
        for (width, height) in [(80, 24), (120, 35)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app, &Palette::dark()))
                .expect("draw");
            let row = |y| {
                (0..width)
                    .map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .expect("cell")
                            .symbol()
                            .to_string()
                    })
                    .collect::<String>()
            };
            let title = row(0);
            let status = row(1);
            let guide = row(height - 2);
            assert!(guide.contains("hjkl move"), "{guide}");
            if width == 120 {
                for control in ["t Size", "i Hidden", "d Disk", "Depth 3"] {
                    assert!(title.contains(control), "{title}");
                }
                assert!(!status.contains("t Size"), "{status}");
                let mut last = 0;
                for hint in [
                    "? help",
                    "Space mark",
                    "Enter open",
                    "Backspace up",
                    "c review",
                    "hjkl move",
                    "/ filter",
                    "[ ] depth",
                    "V volumes",
                    "r rescan",
                    "w look",
                    "v list",
                ] {
                    let position = guide.find(hint).expect("footer hint");
                    assert!(position >= last, "footer order: {guide}");
                    last = position + hint.len();
                }
            } else {
                assert!(!title.contains("t Size"), "{title}");
                assert!(status.contains("t Size"), "{status}");
            }
        }
    }

    #[test]
    fn screens_render_at_compact_and_wide_sizes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir(root.join("cache")).expect("mkdir");
        std::fs::write(root.join("cache/large"), vec![b'x'; 8192])
            .expect("write");
        std::fs::write(root.join("file"), vec![b'y'; 2048]).expect("write");
        let tree = scan(root, ScanOptions::default()).expect("scan");
        let mut app = App::new(root.to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.selected = Some(vec![0]);
        app.tree = Some(tree);
        app.space = Some(disktree_core::space::SpaceInfo {
            total: 100,
            free: 25,
            available: 25,
        });
        app.marks.push(Target {
            path: root.join("cache"),
            bytes: 8192,
            is_dir: true,
            hidden: false,
        });
        for (width, height) in
            [(45, 13), (60, 24), (65, 24), (80, 24), (120, 35)]
        {
            let device = if width >= 80 {
                "/dev/mapper/ubuntu--vg-ubuntu--lv"
            } else {
                "/dev/test0"
            };
            app.device = Some(device.to_string());
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            for screen in [
                Screen::Explore,
                Screen::Review,
                Screen::Confirm,
                Screen::Running,
                Screen::Done,
                Screen::Help,
                Screen::Insights,
                Screen::SavePrompt,
                Screen::Volumes,
            ] {
                app.screen = screen;
                terminal
                    .draw(|frame| draw(frame, &app, &Palette::dark()))
                    .expect("draw");
                let output: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect();
                assert!(output.contains("disktree"));
                if screen == Screen::Explore {
                    assert!(output.contains("hjkl move"), "width {width}");
                    for control in ["t Size", "i Hidden", "d Disk"] {
                        assert!(
                            output.contains(control),
                            "width {width}: {control}"
                        );
                    }
                    assert!(output.contains(&format!("DISK {device}")));
                    assert!(output.contains("1 marked"));
                    if width <= 80 {
                        assert!(output.contains("75% used"));
                        assert!(output.contains("? help"), "width {width}");
                        if width == 80 {
                            assert!(output.contains("v list"));
                            assert!(output.contains("w look"));
                        }
                        if (60..=80).contains(&width) {
                            let legend: String = (0..width)
                                .map(|x| {
                                    terminal
                                        .backend()
                                        .buffer()
                                        .cell((x, 2))
                                        .expect("legend cell")
                                        .symbol()
                                        .to_string()
                                })
                                .collect();
                            for label in [
                                "Code", "Agent", "Tools", "Sync", "Git",
                                "Media", "Docs", "Cache",
                            ] {
                                assert!(legend.contains(label), "{legend}");
                            }
                            assert!(legend.contains(if width == 60 {
                                "Rclm"
                            } else {
                                "Reclaim"
                            }));
                        }
                        let key = terminal
                            .backend()
                            .buffer()
                            .cell((2, height - 2))
                            .expect("guide key");
                        assert_eq!(key.fg, ACCENT);
                        assert_eq!(key.bg, PANEL);
                    }
                    if width >= 80 {
                        assert!(output.contains("25 B free"));
                        assert!(output.contains("75 B used / 100 B total"));
                        assert!(output.contains("projected"));
                    }
                }
                if width == 45 && screen == Screen::Review {
                    assert!(output.contains("a agent"));
                    assert!(output.contains("p delete"));
                }
                if width == 45 && screen == Screen::Help {
                    assert!(output.contains("Space mark"));
                }
            }
        }
    }

    #[test]
    fn compact_header_shows_all_changed_control_states() {
        let temp = tempfile::tempdir().expect("tempdir");
        let options = ScanOptions {
            metric: disktree_core::tree::Metric::Files,
            apparent_size: true,
            include_hidden: false,
            ..ScanOptions::default()
        };
        let mut app = App::new(temp.path().to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        let mut terminal =
            Terminal::new(TestBackend::new(45, 13)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("draw");
        let header: String = (0..45)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 1))
                    .expect("header cell")
                    .symbol()
                    .to_string()
            })
            .collect();
        for control in ["t Files", "i Visible", "d Apparent"] {
            assert!(header.contains(control), "{header}");
        }
    }

    #[test]
    fn age_mode_names_its_scale_at_narrow_and_wide_sizes() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("fresh"), b"fresh")
            .expect("fresh file");
        std::fs::write(temp.path().join("old"), b"old").expect("old file");
        let mut tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let scanned_at = 100_000_000;
        for child in &mut tree.children {
            child.modified = if child.name.as_ref() == "fresh" {
                scanned_at - 3 * 86_400
            } else {
                scanned_at - 500 * 86_400
            };
        }
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.mode = ViewMode::Age;
        app.scanned_at = scanned_at;
        app.selected = Some(vec![0]);
        for (width, height) in [(45, 13), (80, 24), (120, 35)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app, &Palette::dark()))
                .expect("draw");
            let output: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(output.contains("t Age"), "width {width}: {output}");
            if width == 45 {
                assert!(output.contains("[Week]"), "{output}");
                assert!(output.contains("[Older]"), "{output}");
            } else {
                assert!(output.contains("This week"), "{output}");
                assert!(output.contains("Older"), "{output}");
            }
        }
    }

    #[test]
    fn volume_picker_shows_drives_and_windows_paths_read_normally() {
        assert_eq!(readable_path(r"\\?\R:\"), r"R:\");
        assert_eq!(readable_path(r"\\?\UNC\server\share"), r"\\server\share");
        let temp = tempfile::tempdir().expect("tempdir");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.screen = Screen::Volumes;
        app.volumes = vec![disktree_core::space::Volume {
            point: Path::new(r"R:\").to_path_buf(),
            device: None,
            space: Some(disktree_core::space::SpaceInfo {
                total: 100,
                free: 25,
                available: 25,
            }),
        }];
        let mut terminal =
            Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("draw");
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(output.contains("SCAN A VOLUME"));
        assert!(output.contains(r"R:\"));
        assert!(output.contains("25 B free"));
    }

    #[test]
    fn empty_footer_and_insights_leave_room_for_scan_metadata() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("file"), b"small").expect("file");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0]);
        app.progress.files = 1;
        app.progress.dirs = 1;
        app.scan_elapsed = Some(Duration::from_millis(1230));
        let mut terminal =
            Terminal::new(TestBackend::new(120, 35)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("draw");
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(output.contains("Depth 3"));
        assert!(output.contains("scan 2 entries · 1.2s"));
        assert!(!output.contains("marked"));
        assert!(!output.contains("projected"));
        assert!(!output.contains("WORTH A LOOK"));
    }

    #[test]
    fn tile_sizes_keep_units_whole() {
        assert_eq!(
            fit_size_label("980KiB", 5, 2),
            Some(("980K".to_string(), None))
        );
        assert_eq!(
            fit_size_label("1.9MiB", 4, 2),
            Some(("1.9M".to_string(), None))
        );
        assert_eq!(
            fit_size_label("980KiB", 6, 1),
            Some(("980KiB".to_string(), None))
        );
        assert_eq!(
            fit_size_label("980KiB", 3, 2),
            Some(("980".to_string(), Some("KiB".to_string())))
        );
        assert_eq!(fit_size_label("980KiB", 3, 1), None);
        assert_eq!(fit_size_label("980KiB", 2, 2), None);
    }

    #[test]
    fn narrow_disk_line_keeps_the_free_value_whole() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.device = Some("/dev/mapper/ubuntu--vg-ubuntu--lv".to_string());
        let line = compact_disk_line(&app, 34 * 1024 * 1024 * 1024, 43);
        assert!(line.chars().count() <= 43);
        assert!(line.starts_with("DISK /dev/"));
        assert!(line.contains('…'));
        assert!(line.ends_with(" · 34 GiB free"));
    }

    #[test]
    fn sliding_window_reveals_both_ends_with_right_edge_ellipsis() {
        let value = "abcdefghij";
        assert_eq!(sliding_window(value, 5, Duration::ZERO).0, "abcd…");
        assert_eq!(
            sliding_window(value, 5, Duration::from_millis(800)).0,
            "bcde…"
        );
        assert_eq!(
            sliding_window(value, 5, Duration::from_millis(1800)).0,
            "fghij"
        );
        assert_eq!(
            sliding_window(value, 5, Duration::from_millis(2600)).0,
            "abcd…"
        );
        assert_eq!(
            sliding_window(value, 5, Duration::ZERO).1,
            Some(Duration::from_millis(800))
        );
        assert_eq!(
            sliding_window(value, 5, Duration::from_millis(800)).1,
            Some(Duration::from_millis(200))
        );
        assert_eq!(
            sliding_window(value, 5, Duration::from_millis(1800)).1,
            Some(Duration::from_millis(800))
        );
        assert_eq!(end_clip("Mozilla", 5), "Mozi…");
        let long = format!("{}-final-name", "a".repeat(100));
        let tail = sliding_window(&long, 12, Duration::from_millis(5800)).0;
        assert!(tail.ends_with("-final-name"), "{tail}");
        assert!(!tail.contains('…'), "{tail}");
        let unicode = "a👩‍💻bcdef";
        for step in 0..20 {
            let label =
                sliding_window(unicode, 5, Duration::from_millis(step * 200)).0;
            assert!(text_width(&label) <= 5, "{label}");
            assert!(!label.contains('�'), "{label}");
            assert!(!label.contains('…') || label.ends_with('…'), "{label}");
        }
    }

    #[test]
    fn unselected_tile_border_uses_a_console_line_glyph() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 2));
        top_border(&mut buffer, Rect::new(0, 0, 6, 2), ACCENT);
        let border: String = (0..6)
            .map(|x| buffer[(x, 0)].symbol().to_string())
            .collect();
        assert_eq!(border, "──────");
    }

    #[test]
    fn unselected_tile_and_worth_a_look_path_keep_moving() {
        let temp = tempfile::tempdir().expect("tempdir");
        let long_name = "MozillaProfilesAndLocalApplicationCachesWithLongNames";
        for name in ["Selected", long_name] {
            let directory = temp.path().join(name);
            std::fs::create_dir(&directory).expect("mkdir");
            std::fs::write(directory.join("file"), vec![b'x'; 8192])
                .expect("file");
        }
        let tree = scan(
            temp.path(),
            ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
        )
        .expect("scan");
        let mut app =
            App::new(temp.path().to_path_buf(), ScanOptions::default());
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        let long_index = tree
            .children
            .iter()
            .position(|node| node.name.as_ref() == long_name)
            .expect("long node");
        let selected_index = usize::from(long_index == 0);
        app.insights = vec![Candidate {
            crumbs: vec![long_index],
            bytes: 8192,
            finding: Finding::Worktrees {
                count: 1,
                oldest_days: 40,
            },
        }];
        app.tree = Some(tree);
        app.selected = Some(vec![selected_index]);
        app.set_viewport(120, 35);
        let body = app.body_area(app.viewport());
        let (map, side, _) = App::split_explore(body);
        let tile = app
            .tiles(map)
            .iter()
            .find(|tile| {
                matches!(&tile.kind, TileKind::Node { crumbs } if crumbs == &vec![long_index])
            })
            .map(|tile| App::tile_rect(tile, map))
            .expect("unselected tile");
        let worth_y = side.y + 15;
        let mut terminal =
            Terminal::new(TestBackend::new(120, 35)).expect("terminal");
        let row =
            |terminal: &Terminal<TestBackend>, x: u16, y: u16, width: u16| {
                (x..x + width)
                    .map(|column| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((column, y))
                            .expect("cell")
                            .symbol()
                            .to_string()
                    })
                    .collect::<String>()
            };
        app.set_marquee_elapsed_for_test(Duration::ZERO);
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("initial draw");
        let first_tile = row(&terminal, tile.x, tile.y, tile.width);
        let first_worth = row(&terminal, side.x, worth_y, side.width);
        let reason = row(&terminal, side.x, worth_y + 1, side.width);
        assert!(reason.contains("1 worktrees · oldest 40 days"), "{reason}");
        app.set_marquee_elapsed_for_test(Duration::from_millis(1200));
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("later draw");
        let later_tile = row(&terminal, tile.x, tile.y, tile.width);
        let later_worth = row(&terminal, side.x, worth_y, side.width);
        assert_ne!(first_tile, later_tile);
        assert_ne!(first_worth, later_worth);
        assert!(first_tile.contains("Mozilla"), "{first_tile}");
        assert!(first_worth.contains("Mozilla"), "{first_worth}");
        assert!(first_worth.contains('…'), "{first_worth}");
        assert!(later_worth.contains('…'), "{later_worth}");
        assert!(later_worth.contains("8.0KiB"), "{later_worth}");
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ));
        assert!(app.marquee_elapsed() >= Duration::from_millis(1200));
        assert!(app.focused_marquee_elapsed() < Duration::from_millis(100));

        app.screen = Screen::Insights;
        for (width, height) in [(45, 13), (80, 24)] {
            app.set_viewport(width, height);
            app.set_marquee_elapsed_for_test(Duration::ZERO);
            let mut narrow = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            narrow
                .draw(|frame| draw(frame, &app, &Palette::dark()))
                .expect("insights draw");
            let insights: String = narrow
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(insights.contains("WORTH A LOOK"));
            assert!(insights.contains("8.0 KiB"));
            assert!(insights.contains("oldest 40 days"));
            assert!(insights.contains("Mozilla"));
        }
    }

    #[test]
    fn narrow_list_uses_its_legend_row_for_a_fifth_entry() {
        let temp = tempfile::tempdir().expect("tempdir");
        for (name, bytes) in [
            ("Media", 5000),
            ("Documents", 4000),
            ("Projects", 3000),
            ("misc", 2000),
            ("fifth", 1000),
        ] {
            let path = temp.path().join(name);
            std::fs::create_dir(&path).expect("mkdir");
            let file = if name == "Media" {
                path.join("photos/video.mp4")
            } else {
                path.join("file")
            };
            std::fs::create_dir_all(file.parent().expect("parent"))
                .expect("parent dir");
            std::fs::write(file, vec![b'x'; bytes]).expect("file");
        }
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let tree = scan(temp.path(), options.clone()).expect("scan");
        let mut app = App::new(temp.path().to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0]);
        let mut terminal =
            Terminal::new(TestBackend::new(45, 13)).expect("terminal");
        terminal
            .draw(|frame| draw(frame, &app, &Palette::dark()))
            .expect("draw");
        let output: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        let header: String = (0..45)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 1))
                    .expect("header cell")
                    .symbol()
                    .to_string()
            })
            .collect();
        assert!(header.contains("t Size"));
        assert!(header.contains("i Hidden"));
        assert!(header.contains("d Apparent"));
        assert!(output.contains("files"));
        assert!(output.contains("dirs"));
        assert!(output.contains("Media [Media]"), "{output}");
        assert!(output.contains("fifth [Other]"), "{output}");
        assert!(!output.contains("Reclaimable"));
    }

    #[test]
    fn nested_tiles_remain_visible_at_terminal_widths() {
        let temp = tempfile::tempdir().expect("tempdir");
        for relative in
            ["Projects/app/src/main.rs", "Projects/site/pages/index.html"]
        {
            let path = temp.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent"))
                .expect("mkdir");
            std::fs::write(path, vec![b'x'; 8192]).expect("write");
        }
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let tree = scan(temp.path(), options.clone()).expect("scan");
        let mut app = App::new(temp.path().to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.selected = Some(vec![0]);

        for (width, height) in [(80, 24), (120, 35)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))
                .expect("terminal");
            terminal
                .draw(|frame| draw(frame, &app, &Palette::dark()))
                .expect("draw");
            let output: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(output.contains("app"), "app tile at {width} columns");
            assert!(output.contains("site"), "site tile at {width} columns");
            if width >= 120 {
                assert!(output.contains("src"), "third level at {width}");
                assert!(output.contains("pages"), "third level at {width}");
            }
        }
    }

    #[test]
    fn compact_mosaic_shows_children_with_several_peer_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        for relative in [
            "Projects/app/file",
            ".cache/browser/file",
            "Media/video/file",
            "Documents/reports/file",
            "Downloads/tmp/file",
        ] {
            let path = temp.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent"))
                .expect("mkdir");
            std::fs::write(path, vec![b'x'; 8192]).expect("write");
        }
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let tree = scan(temp.path(), options.clone()).expect("scan");
        let mut app = App::new(temp.path().to_path_buf(), options);
        if let Some(scan) = app.scan.take() {
            scan.cancel();
        }
        app.tree = Some(tree);
        app.view_depth = 2;
        app.set_viewport(80, 24);
        let map = App::split_explore(app.body_area(app.viewport())).0;
        let root = app.tree.as_ref().expect("tree");
        let visible: Vec<_> = app
            .tiles(map)
            .iter()
            .filter_map(|tile| match &tile.kind {
                TileKind::Node { crumbs } if crumbs.len() == 2 => {
                    root.resolve(crumbs).map(|node| node.name.to_string())
                }
                _ => None,
            })
            .collect();
        for child in ["app", "browser", "video", "reports", "tmp"] {
            assert!(visible.iter().any(|name| name == child), "{visible:?}");
        }
    }
}
