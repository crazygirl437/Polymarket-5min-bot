use crate::ui::state::{DashboardState, HealthStatus, PriceDir};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{block::Position, Block, Borders, Cell, Gauge, Paragraph, Row, Sparkline, Table},
    Frame,
};

fn border_style() -> Style {
    Style::default().fg(Color::Rgb(40, 40, 60))
}

fn title_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

fn label_style() -> Style {
    Style::default().fg(Color::Gray)
}

fn profit_color(pnl: f64) -> Color {
    if pnl > 0.0 {
        Color::LightGreen
    } else if pnl < 0.0 {
        Color::Red
    } else {
        Color::White
    }
}

fn dir_style(dir: PriceDir) -> Style {
    match dir {
        PriceDir::Up => Style::default().fg(Color::Green),
        PriceDir::Down => Style::default().fg(Color::Red),
        PriceDir::Flat => Style::default().fg(Color::DarkGray),
    }
}

fn health_color(status: HealthStatus) -> Color {
    match status {
        HealthStatus::Ok => Color::Green,
        HealthStatus::Warn => Color::Yellow,
        HealthStatus::Err => Color::Red,
    }
}

pub fn draw(frame: &mut Frame, app: &DashboardState) {
    let area = frame.area();
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(11),
            Constraint::Min(8),
            Constraint::Length(9),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, root[0], app);
    draw_hero_profit(frame, root[1], app);
    draw_body(frame, root[2], app);
    draw_ticker(frame, root[3], app);
    draw_status_bar(frame, root[4], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style())
        .title(Span::styled(
            " POLYMARKET ARB BOT ",
            Style::default().fg(Color::Cyan).bold(),
        ))
        .title_alignment(Alignment::Center);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35),
            Constraint::Percentage(35),
            Constraint::Percentage(30),
        ])
        .split(inner);

    let status = if app.connected {
        Span::styled(" ● LIVE ", Style::default().fg(Color::Green).bold())
    } else {
        Span::styled(" ○ INIT ", Style::default().fg(Color::Yellow))
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            status,
            Span::styled(format!("  uptime {}", app.uptime()), Style::default().fg(Color::White)),
        ])),
        cols[0],
    );

    frame.render_widget(
        Paragraph::new("Automated YES+NO Spread Arbitrage")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Blue).italic()),
        cols[1],
    );

    frame.render_widget(
        Paragraph::new(format!("UTC {}", app.utc_now().format("%H:%M:%S")))
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::White)),
        cols[2],
    );
}

fn draw_hero_profit(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let pulse = app.profit_pulse > 0;
    let bg = if pulse {
        Color::Rgb(0, 55, 20)
    } else {
        Color::Rgb(10, 10, 25)
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if pulse {
            Style::default().fg(Color::LightGreen)
        } else {
            Style::default().fg(Color::Yellow)
        })
        .style(Style::default().bg(bg))
        .title(Span::styled(
            " 💰 SESSION PROFIT 💰 ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .title_alignment(Alignment::Center);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(2),
        ])
        .split(inner);

    let sign = if app.session_pnl >= 0.0 { "+" } else { "" };
    let hero = format!("{sign}${:.2}", app.session_pnl);
    let dollar_glow = if pulse { " $$$ " } else { "  $  " };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(dollar_glow, Style::default().fg(Color::Yellow).bold()),
            Span::styled(
                hero,
                Style::default()
                    .fg(profit_color(app.session_pnl))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(dollar_glow, Style::default().fg(Color::Yellow).bold()),
        ]))
        .alignment(Alignment::Center),
        rows[0],
    );

    let last_trade = if app.last_trade_pnl > 0.0 && !app.last_trade_symbol.is_empty() {
        format!(
            "▲ +${:.2} last trade ({})",
            app.last_trade_pnl, app.last_trade_symbol
        )
    } else if app.total_trades > 0 {
        "Scanning for next opportunity…".to_string()
    } else {
        "Waiting for first arbitrage capture…".to_string()
    };

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            last_trade,
            Style::default().fg(Color::Green),
        )))
        .alignment(Alignment::Center),
        rows[1],
    );

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(rows[2]);

    let window_sign = if app.window_pnl >= 0.0 { "+" } else { "" };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Window PnL  ", label_style()),
            Span::styled(
                format!("{window_sign}${:.2}", app.window_pnl),
                Style::default().fg(profit_color(app.window_pnl)).bold(),
            ),
        ]))
        .alignment(Alignment::Center),
        cols[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Trades  ", label_style()),
            Span::styled(
                format!(
                    "{} / {} wins",
                    app.successful_trades, app.total_trades
                ),
                Style::default().fg(Color::White).bold(),
            ),
            Span::styled(
                format!("  ({:.0}%)", app.win_rate()),
                Style::default().fg(Color::Cyan),
            ),
        ]))
        .alignment(Alignment::Center),
        cols[1],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Best trade  ", label_style()),
            Span::styled(
                format!("+${:.2}", app.best_trade),
                Style::default().fg(Color::LightGreen).bold(),
            ),
        ]))
        .alignment(Alignment::Center),
        cols[2],
    );

    let curve_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[3]);

    draw_sparkline_block(
        frame,
        curve_cols[0],
        " SESSION PNL ",
        &app.pnl_sparkline,
        Color::LightGreen,
    );
    draw_sparkline_block(
        frame,
        curve_cols[1],
        " WINDOW PNL ",
        &app.window_pnl_sparkline,
        Color::Yellow,
    );
}

fn draw_body(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    draw_markets_panel(frame, cols[0], app);
    draw_side_panel(frame, cols[1], app);
}

fn draw_markets_panel(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(48),
            Constraint::Length(5),
            Constraint::Min(4),
        ])
        .split(area);

    draw_market_table(frame, rows[0], app);
    draw_global_charts(frame, rows[1], app);
    draw_market_edge_curves(frame, rows[2], app);
}

fn draw_market_table(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let header = Row::new(vec!["SYM", "YES", "NO", "Σ", "EDGE"])
        .style(Style::default().fg(Color::Yellow).bold())
        .height(1);

    let table_rows: Vec<Row> = if app.markets.is_empty() {
        vec![Row::new(vec![Cell::from("—"), Cell::from("waiting…"), Cell::from(""), Cell::from(""), Cell::from("")])]
    } else {
        app.markets
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let total = m.yes_price + m.no_price;
                let edge = app.profit_pct(m);
                let edge_str = if edge > 0.0 {
                    format!("+{edge:.2}%")
                } else {
                    "—".to_string()
                };

                let mut style = Style::default().fg(Color::White);
                if m.is_arb && app.flash_arb {
                    style = style.bg(Color::Rgb(0, 48, 0));
                }
                if i == app.selected_market {
                    style = style.fg(Color::Cyan).bold();
                }

                Row::new(vec![
                    Cell::from(m.symbol.clone()),
                    Cell::from(format!("{:.3}{}", m.yes_price, m.yes_dir.arrow()))
                        .style(dir_style(m.yes_dir)),
                    Cell::from(format!("{:.3}{}", m.no_price, m.no_dir.arrow()))
                        .style(dir_style(m.no_dir)),
                    Cell::from(format!("{total:.3}")),
                    Cell::from(edge_str).style(if edge > 0.3 {
                        Style::default().fg(Color::LightGreen).bold()
                    } else {
                        Style::default().fg(Color::DarkGray)
                    }),
                ])
                .style(style)
                .height(1)
            })
            .collect()
    };

    let table = Table::new(
        table_rows,
        [
            Constraint::Length(5),
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Length(7),
            Constraint::Min(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(" LIVE MARKETS ")
            .borders(Borders::ALL)
            .border_style(border_style())
            .title_style(title_style()),
    );

    frame.render_widget(table, area);
}

fn draw_sparkline_block(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    data: &[u64],
    color: Color,
) {
    let spark = Sparkline::default()
        .block(
            Block::default()
                .title(title)
                .title_style(title_style())
                .borders(Borders::ALL)
                .border_style(border_style())
                .title_position(Position::Top),
        )
        .data(data)
        .style(Style::default().fg(color));
    frame.render_widget(spark, area);
}

fn draw_global_charts(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    draw_sparkline_block(
        frame,
        cols[0],
        " EDGE ",
        &app.edge_sparkline,
        Color::Magenta,
    );
    draw_sparkline_block(
        frame,
        cols[1],
        " EXPOSURE ",
        &app.exposure_sparkline,
        Color::Cyan,
    );
    draw_sparkline_block(
        frame,
        cols[2],
        " SCAN RATE ",
        &app.scan_rate_sparkline,
        Color::Blue,
    );
    draw_sparkline_block(
        frame,
        cols[3],
        " PNL FLOW ",
        &app.pnl_sparkline,
        Color::LightGreen,
    );
}

fn draw_market_edge_curves(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let block = Block::default()
        .title(" MARKET EDGE CURVES ")
        .title_style(title_style())
        .borders(Borders::ALL)
        .border_style(border_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.markets.is_empty() {
        frame.render_widget(
            Paragraph::new("Waiting for market data…").style(label_style()),
            inner,
        );
        return;
    }

    let row_h = inner.height.saturating_sub(1) / app.markets.len().max(1) as u16;
    let row_h = row_h.max(2);

    for (i, market) in app.markets.iter().enumerate() {
        let y = inner.y + (i as u16 * row_h);
        if y >= inner.bottom() {
            break;
        }
        let row_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: row_h.min(inner.bottom().saturating_sub(y)),
        };

        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(5), Constraint::Min(8), Constraint::Length(8)])
            .split(row_area);

        let edge = app.profit_pct(market);
        let edge_label = if edge > 0.0 {
            format!("+{edge:.2}%")
        } else {
            "—".to_string()
        };

        let sym_style = if i == app.selected_market {
            Style::default().fg(Color::Cyan).bold()
        } else if market.is_arb {
            Style::default().fg(Color::LightGreen).bold()
        } else {
            Style::default().fg(Color::Yellow)
        };

        frame.render_widget(Paragraph::new(market.symbol.as_str()).style(sym_style), cols[0]);

        let curve_color = if market.is_arb {
            Color::LightGreen
        } else {
            Color::Rgb(80, 120, 200)
        };
        let spark = Sparkline::default()
            .block(Block::default().borders(Borders::NONE))
            .data(market.sparkline.as_slice())
            .style(Style::default().fg(curve_color));
        frame.render_widget(spark, cols[1]);

        frame.render_widget(
            Paragraph::new(edge_label)
                .alignment(Alignment::Right)
                .style(if edge > 0.3 {
                    Style::default().fg(Color::Green).bold()
                } else {
                    label_style()
                }),
            cols[2],
        );
    }
}

fn draw_side_panel(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(42),
            Constraint::Length(4),
            Constraint::Min(4),
        ])
        .split(area);

    draw_risk(frame, rows[0], app);
    draw_sparkline_block(
        frame,
        rows[1],
        " EXPOSURE TREND ",
        &app.exposure_sparkline,
        Color::Cyan,
    );
    draw_system(frame, rows[2], app);
}

fn draw_risk(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let block = Block::default()
        .title(" RISK ")
        .borders(Borders::ALL)
        .border_style(border_style())
        .title_style(title_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(3), Constraint::Min(2)])
        .split(inner);

    frame.render_widget(
        Paragraph::new(format!(
            "Exposure ${:.2} / ${:.0}",
            app.exposure, app.exposure_limit
        )),
        rows[0],
    );

    let pct = app.exposure_pct();
    let gauge_color = if pct > 90.0 {
        Color::Red
    } else if pct > 75.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::NONE))
        .gauge_style(Style::default().fg(gauge_color))
        .percent(pct.clamp(0.0, 100.0) as u16)
        .label(format!("{pct:.0}%"));
    frame.render_widget(gauge, rows[1]);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Positions: {}", app.positions)),
            Line::from(format!(
                "Last trade: {:.0}s ago",
                app.last_trade_secs.min(999.0)
            )),
        ]),
        rows[2],
    );
}

fn draw_system(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let lines: Vec<Line> = app
        .services
        .iter()
        .map(|svc| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", svc.status.dot()),
                    Style::default().fg(health_color(svc.status)),
                ),
                Span::styled(format!("{:<10}", svc.name), Style::default().fg(Color::White)),
                Span::styled(
                    if svc.latency_ms > 0 {
                        format!("{}ms", svc.latency_ms)
                    } else {
                        "ok".to_string()
                    },
                    label_style(),
                ),
            ])
        })
        .chain(std::iter::once(Line::from(vec![
            Span::styled("Merge  ", label_style()),
            Span::styled(&app.merge_status, Style::default().fg(Color::Cyan)),
        ])))
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" SYSTEM ")
                .borders(Borders::ALL)
                .border_style(border_style())
                .title_style(title_style()),
        ),
        area,
    );
}

fn draw_ticker(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let max_lines = inner_h.saturating_sub(1).max(4);
    let recent = app.recent_events(max_lines);
    let last = recent.len().saturating_sub(1);

    let lines: Vec<Line> = recent
        .into_iter()
        .enumerate()
        .map(|(i, msg)| {
            let style = if i == last {
                Style::default().fg(Color::White)
            } else if msg.contains('💰') || msg.contains("ARB") || msg.contains('⚡') {
                Style::default().fg(Color::LightGreen)
            } else if msg.contains('❌') || msg.contains('⚠') {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            Line::from(vec![
                Span::styled(" › ", Style::default().fg(Color::Rgb(60, 60, 80))),
                Span::styled(msg, style),
            ])
        })
        .collect();

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" EVENT LOG ")
                .title_style(title_style())
                .borders(Borders::ALL)
                .border_style(border_style()),
        ),
        area,
    );
}

fn draw_status_bar(frame: &mut Frame, area: Rect, app: &DashboardState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("WINDOW ", label_style()),
                Span::styled(&app.window_label, Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::styled("ends ", label_style()),
                Span::styled(app.window_countdown(), Style::default().fg(Color::Yellow).bold()),
            ]),
        ]),
        cols[0],
    );

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("scans ", label_style()),
                Span::styled(format!("{}", app.arb_scans), Style::default().fg(Color::White).bold()),
            ]),
            Line::from(vec![
                Span::styled("mode ", label_style()),
                Span::styled(&app.order_mode, Style::default().fg(Color::Magenta)),
            ]),
        ])
        .alignment(Alignment::Center),
        cols[1],
    );

    frame.render_widget(
        Paragraph::new("q quit bot")
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::DarkGray)),
        cols[2],
    );
}
