use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use glib;
use gtk;
use gtk::prelude::*;

use log::{debug, error, warn};
use nvim_rs::{Tabpage, Window as NvimWindow};

use crate::nvim_bridge::{
    CmdlineBlockAppend, CmdlineBlockShow, CmdlinePos, CmdlineShow,
    CmdlineSpecialChar, DefaultColorsSet, GnvimEvent, GridCursorGoto,
    GridLineSegment, GridResize, GridScroll, HlAttrDefine, HlGroupSet,
    ModeChange, ModeInfo, ModeInfoSet, MsgSetPos, Notify, OptionSet,
    PopupmenuShow, RedrawEvent, TablineUpdate, WindowExternalPos,
    WindowFloatPos, WindowPos, WindowViewport,
};
use crate::nvim_gio::GioNeovim;
use crate::ui::cmdline::Cmdline;
use crate::ui::color::{HlDefs, HlGroup};
use crate::ui::common::spawn_local;
#[cfg(feature = "libwebkit2gtk")]
use crate::ui::cursor_tooltip::{CursorTooltip, Gravity};
use crate::ui::font::Font;
use crate::ui::grid::{Grid, GridMetrics};
use crate::ui::popupmenu::Popupmenu;
use crate::ui::tabline::Tabline;
use crate::ui::window::{MsgWindow, Window};

pub(crate) type Windows = HashMap<i64, Window>;
pub(crate) type Grids = HashMap<i64, Grid>;

pub(crate) struct ResizeOptions {
    pub font: Font,
    pub line_space: i64,
}

/// Internal structure for `UI` to work on.
pub(crate) struct UIState {
    pub css_provider: gtk::CssProvider,
    pub windows: Windows,
    /// Container for non-floating windows.
    pub windows_container: gtk::Fixed,
    /// Container for floating windows.
    pub windows_float_container: gtk::Fixed,
    /// Container for the msg window/grid.
    pub msg_window_container: gtk::Fixed,
    /// Window for our messages grid.
    pub msg_window: MsgWindow,
    /// All grids currently in the UI.
    pub grids: Grids,
    /// Highlight definitions.
    pub hl_defs: HlDefs,
    /// Mode infos. When a mode is activated, the activated mode is passed
    /// to the gird(s).
    pub mode_infos: Vec<ModeInfo>,
    /// Current mode.
    pub current_mode: Option<ModeInfo>,
    /// Id of the current active grid.
    pub current_grid: i64,

    pub popupmenu: Popupmenu,
    pub cmdline: Cmdline,
    pub tabline: Tabline,
    #[cfg(feature = "libwebkit2gtk")]
    pub cursor_tooltip: CursorTooltip,

    pub wildmenu_shown: bool,

    /// Overlay contains our grid(s) and popupmenu.
    #[allow(unused)]
    pub overlay: gtk::Overlay,

    /// Source id for delayed call to ui_try_resize.
    pub resize_source_id: Rc<RefCell<Option<glib::SourceId>>>,
    /// Resize options that is some if a resize should be send to nvim on flush.
    pub resize_on_flush: Option<ResizeOptions>,

    /// Flag for flush to update GUI colors on components that depend on
    /// highlight defs and groups.
    pub hl_changed: bool,

    pub font: Font,
    pub line_space: i64,
}

impl UIState {
    pub fn handle_notify(
        &mut self,
        window: &gtk::ApplicationWindow,
        notify: Notify,
        nvim: &GioNeovim,
    ) {
        match notify {
            Notify::RedrawEvent(events) => {
                events.into_iter().for_each(|e| {
                    self.handle_redraw_event(window, e, &nvim);
                });
            }
            Notify::GnvimEvent(event) => match event {
                Ok(event) => self.handle_gnvim_event(&event, nvim),
                Err(err) => {
                    let nvim = nvim.clone();
                    let msg = format!(
                        "echom \"Failed to parse gnvim notify: '{}'\"",
                        err
                    );
                    spawn_local(async move {
                        if let Err(err) = nvim.command(&msg).await {
                            error!("Failed to execute nvim command: {}", err)
                        }
                    });
                }
            },
        }
    }

    fn set_title(&mut self, window: &gtk::ApplicationWindow, title: &str) {
        window.set_title(title);
    }

    fn grid_cursor_goto(
        &mut self,
        GridCursorGoto {
            grid: grid_id,
            row,
            col,
        }: GridCursorGoto,
    ) {
        // Gird cursor goto sets the current cursor to grid_id,
        // so we'll need to handle that here...
        let grid = if grid_id != self.current_grid {
            // ...so if the grid_id is not same as the self tells us,
            // set the previous current grid to inactive self.
            let grid = self.grids.get(&self.current_grid).unwrap();

            grid.set_active(false);
            grid.tick(); // Trick the grid to invalide the cursor's rect.

            self.current_grid = grid_id;

            // And set the new current grid to active.
            let grid = self.grids.get(&grid_id).unwrap();
            grid.set_active(true);
            grid
        } else {
            self.grids.get(&grid_id).unwrap()
        };

        // And after all that, set the current grid's cursor position.
        grid.cursor_goto(row, col);
    }

    fn grid_resize(
        &mut self,
        e: GridResize,
        window: &gtk::ApplicationWindow,
        nvim: &GioNeovim,
    ) {
        let win = window.get_window().unwrap();
        if let Some(grid) = self.grids.get(&e.grid) {
            grid.resize(&win, e.width, e.height, &self.hl_defs);

            // If the grid is in a window (which is likely), resize the window
            // to match the grid's size.
            if let Some(ref w) =
                self.windows.values().find(|w| w.grid_id == grid.id)
            {
                let grid_metrics = grid.get_grid_metrics();
                w.resize((
                    grid_metrics.width.ceil() as i32,
                    grid_metrics.height.ceil() as i32,
                ));
            }
        } else {
            let grid = Grid::new(
                e.grid,
                &window.get_window().unwrap(),
                self.font.clone(),
                self.line_space,
                e.width as usize,
                e.height as usize,
                &self.hl_defs,
            );

            if let Some(ref mode) = self.current_mode {
                grid.set_mode(&mode);
            }
            grid.resize(&win, e.width, e.height, &self.hl_defs);
            attach_grid_events(&grid, nvim.clone());
            self.grids.insert(e.grid, grid);
        }
    }

    fn grid_line(&mut self, line: GridLineSegment) {
        let grid = self.grids.get(&line.grid).unwrap();
        grid.put_line(line, &self.hl_defs);
    }

    fn grid_clear(&mut self, grid: &i64) {
        let grid = self.grids.get(grid).unwrap();
        grid.clear(&self.hl_defs);
    }

    fn grid_destroy(&mut self, grid: &i64) {
        // Drop grid.
        if self.grids.remove(grid).is_none() {
            warn!(
                "Nvim instructed to close a grid that we don't have (grid: {})",
                grid
            );
        }
        if self.windows.contains_key(grid) {
            self.windows.remove(grid).unwrap(); // Drop window that the grid belongs to.
        }

        // Make the current grid to point to the default grid. We relay on the fact
        // that current_grid is always pointing to a existing grid.
        self.current_grid = 1;
    }

    fn grid_scroll(&mut self, info: GridScroll, nvim: &GioNeovim) {
        let grid = self.grids.get(&info.grid).unwrap();
        grid.scroll(info.reg, info.rows, info.cols, &self.hl_defs);

        // Since nvim doesn't have its own 'scroll' autocmd, we'll
        // have to do it on our own. This use useful for the cursor tooltip.
        let nvim = nvim.clone();
        spawn_local(async move {
            if let Err(err) = nvim.command("if exists('#User#GnvimScroll') | doautocmd User GnvimScroll | endif").await {
                error!("GnvimScroll error: {:?}", err);
            }
        });
    }

    fn default_colors_set(
        &mut self,
        DefaultColorsSet { fg, bg, sp }: DefaultColorsSet,
    ) {
        self.hl_defs.default_fg = fg;
        self.hl_defs.default_bg = bg;
        self.hl_defs.default_sp = sp;

        {
            // NOTE(ville): Not sure if these are actually needed.
            let hl = self.hl_defs.get_mut(&0).unwrap();
            hl.foreground = Some(fg);
            hl.background = Some(bg);
            hl.special = Some(sp);
        }

        for grid in self.grids.values() {
            grid.redraw(&self.hl_defs);
        }

        #[cfg(feature = "libwebkit2gtk")]
        self.cursor_tooltip.set_colors(fg, bg);

        self.hl_changed = true;
    }

    fn hl_attr_define(&mut self, HlAttrDefine { id, hl }: HlAttrDefine) {
        self.hl_defs.insert(id, hl);
    }

    fn hl_group_set(&mut self, evt: HlGroupSet) {
        match evt.name.as_str() {
            "Pmenu" => {
                self.hl_defs.set_hl_group(HlGroup::Pmenu, evt.hl_id);
                self.hl_defs.set_hl_group(HlGroup::Wildmenu, evt.hl_id)
            }
            "PmenuSel" => {
                self.hl_defs.set_hl_group(HlGroup::PmenuSel, evt.hl_id);
                self.hl_defs.set_hl_group(HlGroup::WildmenuSel, evt.hl_id)
            }
            "TabLine" => self.hl_defs.set_hl_group(HlGroup::Tabline, evt.hl_id),
            "TabLineSel" => {
                self.hl_defs.set_hl_group(HlGroup::TablineSel, evt.hl_id);
                self.hl_defs.set_hl_group(HlGroup::CmdlineBorder, evt.hl_id)
            }
            "TabLineFill" => {
                self.hl_defs.set_hl_group(HlGroup::TablineFill, evt.hl_id)
            }
            "Normal" => self.hl_defs.set_hl_group(HlGroup::Cmdline, evt.hl_id),
            "MsgSeparator" => {
                self.hl_defs.set_hl_group(HlGroup::MsgSeparator, evt.hl_id)
            }
            _ => None,
        };

        self.hl_changed = true;
    }

    fn option_set(&mut self, opt: OptionSet) {
        match opt {
            OptionSet::GuiFont(font) => {
                let font = Font::from_guifont(&font).unwrap_or(Font::default());

                self.font = font.clone();

                let mut opts =
                    self.resize_on_flush.take().unwrap_or_else(|| {
                        let grid = self.grids.get(&1).unwrap();
                        ResizeOptions {
                            font: grid.get_font(),
                            line_space: grid.get_line_space(),
                        }
                    });

                opts.font = font;

                self.resize_on_flush = Some(opts);
            }
            OptionSet::LineSpace(val) => {
                self.line_space = val;
                let mut opts =
                    self.resize_on_flush.take().unwrap_or_else(|| {
                        let grid = self.grids.get(&1).unwrap();
                        ResizeOptions {
                            font: grid.get_font(),
                            line_space: grid.get_line_space(),
                        }
                    });

                opts.line_space = val;

                self.resize_on_flush = Some(opts);
            }
            OptionSet::NotSupported(name) => {
                debug!("Not supported option set: {}", name);
            }
        }
    }

    fn mode_info_set(&mut self, ModeInfoSet { mode_info, .. }: ModeInfoSet) {
        self.mode_infos = mode_info.clone();
    }

    fn mode_change(&mut self, ModeChange { index, .. }: ModeChange) {
        let mode = self.mode_infos.get(index as usize).unwrap();
        self.current_mode = Some(mode.clone());
        // Broadcast the mode change to all grids.
        // TODO(ville): It might be enough to just set the mode to the
        //              current active grid.
        for grid in self.grids.values() {
            grid.set_mode(mode);
        }
    }

    fn set_busy(&mut self, busy: bool) {
        for grid in self.grids.values() {
            grid.set_busy(busy);
        }
    }

    fn flush(&mut self, nvim: &GioNeovim, window: &gtk::ApplicationWindow) {
        for grid in self.grids.values() {
            grid.flush(&self.hl_defs);
        }

        if let Some(opts) = self.resize_on_flush.take() {
            let win = window.get_window().unwrap();
            for grid in self.grids.values() {
                grid.update_cell_metrics(
                    opts.font.clone(),
                    opts.line_space,
                    &win,
                );
            }

            let grid = self.grids.get(&1).unwrap();
            let (cols, rows) = grid.calc_size();

            // Cancel any possible delayed call for ui_try_resize.
            let mut id = self.resize_source_id.borrow_mut();
            if let Some(id) = id.take() {
                glib::source::source_remove(id);
            }

            let nvim = nvim.clone();
            spawn_local(async move {
                if let Err(err) =
                    nvim.ui_try_resize(cols as i64, rows as i64).await
                {
                    error!("Error: failed to resize nvim ({:?})", err);
                }
            });

            self.popupmenu.set_font(opts.font.clone(), &self.hl_defs);
            self.cmdline.set_font(opts.font.clone(), &self.hl_defs);
            self.tabline.set_font(opts.font.clone(), &self.hl_defs);
            #[cfg(feature = "libwebkit2gtk")]
            self.cursor_tooltip.set_font(opts.font.clone());

            self.cmdline.set_line_space(opts.line_space);
            self.popupmenu
                .set_line_space(opts.line_space, &self.hl_defs);
            self.tabline.set_line_space(opts.line_space, &self.hl_defs);
        }

        if self.hl_changed {
            self.popupmenu.set_colors(&self.hl_defs);
            self.tabline.set_colors(&self.hl_defs);
            self.cmdline.set_colors(&self.hl_defs);
            self.cmdline.wildmenu_set_colors(&self.hl_defs);

            let msgsep = self
                .hl_defs
                .get_hl_group(&HlGroup::MsgSeparator)
                .cloned()
                .unwrap_or_default()
                .foreground
                .unwrap_or(self.hl_defs.default_fg);

            let scroll_bg = self
                .hl_defs
                .get_hl_group(&HlGroup::CmdlineBorder)
                .cloned()
                .unwrap_or_default()
                .background
                .unwrap_or(self.hl_defs.default_bg);

            // Set the styles for our main window.
            CssProviderExt::load_from_data(
                &self.css_provider,
                format!(
                    "#root {{
                        background: #{bg};
                    }}

                    frame > border {{
                        border: none;
                    }}

                    #windows-container scrollbar,
                    #windows-container-float scrollbar {{
                        background-color: transparent;
                        border: none;
                    }}

                    #windows-container scrollbar slider,
                    #windows-container-float scrollbar slider {{
                        background-color: rgba({slider_r}, {slider_g}, {slider_b}, 0.5);
                    }}

                    #windows-container scrollbar:hover,
                    #windows-container-float scrollbar:hover {{
                        background-color: #{scroll_bg};
                    }}

                    #windows-container scrollbar:hover slider,
                    #windows-container-float scrollbar:hover slider {{
                        background-color: rgba({slider_r}, {slider_g}, {slider_b}, 1);
                    }}

                    #message-grid-contianer frame.scrolled {{
                        border-top: 1px solid #{msgsep}
                    }}
                    ",
                    bg = self.hl_defs.default_bg.to_hex(),
                    slider_r = self.hl_defs.default_fg.r * 255.0,
                    slider_g = self.hl_defs.default_fg.g * 255.0,
                    slider_b = self.hl_defs.default_fg.b * 255.0,
                    scroll_bg = scroll_bg.to_hex(),
                    msgsep = msgsep.to_hex(),
                )
                .as_bytes(),
            )
            .unwrap();

            self.hl_changed = false;
        }
    }

    fn popupmenu_show(&mut self, popupmenu: PopupmenuShow) {
        if popupmenu.grid == -1 {
            self.wildmenu_shown = true;
            self.cmdline.wildmenu_show(&popupmenu.items)
        } else {
            self.popupmenu.set_items(popupmenu.items, &self.hl_defs);

            let grid = self.grids.get(&self.current_grid).unwrap();
            let mut rect = grid.get_rect_for_cell(popupmenu.row, popupmenu.col);

            let window = self.windows.get(&popupmenu.grid).unwrap();
            rect.x += window.x as i32;
            rect.y += window.y as i32;

            self.popupmenu.set_anchor(rect);
            self.popupmenu
                .select(popupmenu.selected as i32, &self.hl_defs);

            self.popupmenu.show();

            // If the cursor tooltip is visible at the same time, move
            // it out of our way.
            #[cfg(feature = "libwebkit2gtk")]
            {
                if self.cursor_tooltip.is_visible() {
                    if self.popupmenu.is_above_anchor() {
                        self.cursor_tooltip.force_gravity(Some(Gravity::Down));
                    } else {
                        self.cursor_tooltip.force_gravity(Some(Gravity::Up));
                    }

                    self.cursor_tooltip.refresh_position();
                }
            }
        }
    }

    fn popupmenu_hide(&mut self) {
        if self.wildmenu_shown {
            self.cmdline.wildmenu_hide();
        } else {
            self.popupmenu.hide();

            // Undo any force positioning of cursor tool tip that might
            // have occured on popupmenu show.
            #[cfg(feature = "libwebkit2gtk")]
            {
                self.cursor_tooltip.force_gravity(None);
                self.cursor_tooltip.refresh_position();
            }
        }
    }

    fn popupmenu_select(&mut self, selected: i64) {
        if self.wildmenu_shown {
            self.cmdline.wildmenu_select(selected as i32);
        } else {
            self.popupmenu.select(selected as i32, &self.hl_defs);
        }
    }

    fn tabline_update(
        &mut self,
        TablineUpdate { current, tabs }: TablineUpdate,
        nvim: &GioNeovim,
    ) {
        let current = Tabpage::new(current, nvim.clone());
        let tabs = tabs
            .into_iter()
            .map(|(value, name)| (Tabpage::new(value, nvim.clone()), name))
            .collect();
        self.tabline.update(current, tabs);
    }

    fn cmdline_show(&mut self, cmdline_show: CmdlineShow) {
        self.cmdline.show(cmdline_show, &self.hl_defs);
    }

    fn cmdline_hide(&mut self) {
        self.cmdline.hide();
    }

    fn cmdline_pos(&mut self, CmdlinePos { pos, level }: CmdlinePos) {
        self.cmdline.set_pos(pos, level);
    }

    fn cmdline_special_char(&mut self, s: CmdlineSpecialChar) {
        self.cmdline
            .show_special_char(s.character, s.shift, s.level);
    }

    fn cmdline_block_show(&mut self, show: CmdlineBlockShow) {
        self.cmdline.show_block(&show, &self.hl_defs);
    }

    fn cmdline_block_append(&mut self, line: CmdlineBlockAppend) {
        self.cmdline.block_append(line, &self.hl_defs);
    }

    fn cmdline_block_hide(&mut self) {
        self.cmdline.hide_block();
    }

    fn window_pos(
        &mut self,
        evt: WindowPos,
        window: &gtk::ApplicationWindow,
        nvim: &GioNeovim,
    ) {
        let win = window.get_window().unwrap();
        let windows_container = self.windows_container.clone();

        let grid = self.grids.get(&evt.grid).unwrap();
        let css_provider = self.css_provider.clone();
        let window = self
            .windows
            .entry(evt.grid)
            .and_modify(clone!(windows_container => move |w| {
                // Set the parent window's to the windows container if needed.
                w.set_parent(windows_container.upcast());
            }))
            .or_insert_with(|| {
                Window::new(
                    NvimWindow::new(evt.win.clone(), nvim.clone()),
                    windows_container,
                    &grid,
                    Some(css_provider),
                    nvim.clone(),
                )
            });

        let grid_metrics = self.grids.get(&1).unwrap().get_grid_metrics();
        let x = evt.start_col as f64 * grid_metrics.cell_width;
        let y = evt.start_row as f64 * grid_metrics.cell_height;
        let width = evt.width as f64 * grid_metrics.cell_width;
        let height = evt.height as f64 * grid_metrics.cell_height;

        window.set_position(x, y, width, height);
        window.show();

        grid.resize(&win, evt.width, evt.height, &self.hl_defs);
    }

    fn get_float_anchor_pos(&self, evt: &WindowFloatPos) -> (f64, f64) {
        if evt.anchor_grid == evt.grid {
            warn!("Can't use a grid as its own float anchor. Defaulting to base grid.");
        }

        if evt.anchor_grid == 1 || evt.anchor_grid == evt.grid {
            (0.0, 0.0)
        } else {
            let anchor_window = self.windows.get(&evt.anchor_grid).unwrap();
            (anchor_window.x, anchor_window.y)
        }
    }

    fn window_float_pos(&mut self, evt: WindowFloatPos, nvim: &GioNeovim) {
        let anchor_grid = self.grids.get(&evt.anchor_grid).unwrap();

        let (x_offset, y_offset) = self.get_float_anchor_pos(&evt);

        let grid = self.grids.get(&evt.grid).unwrap();
        let windows_float_container = self.windows_float_container.clone();
        let css_provider = self.css_provider.clone();

        let window = self
            .windows
            .entry(evt.grid)
            .and_modify(clone!(windows_float_container => move |w| {
                // Set the parent window's to the float container if needed.
                w.set_parent(windows_float_container.upcast());
            }))
            .or_insert_with(|| {
                Window::new(
                    NvimWindow::new(evt.win.clone(), nvim.clone()),
                    windows_float_container,
                    &grid,
                    Some(css_provider),
                    nvim.clone(),
                )
            });

        let anchor_metrics = anchor_grid.get_grid_metrics();
        let grid_metrics = grid.get_grid_metrics();

        let (x, y) = win_float_anchor_pos(
            &evt,
            &anchor_metrics,
            (grid_metrics.width, grid_metrics.height),
            (x_offset, y_offset),
        );

        let base_grid = self.grids.get(&1).unwrap();
        let base_metrics = base_grid.get_grid_metrics();

        let new_size =
            win_float_adjust_size(&grid_metrics, &base_metrics, (x, y));

        if new_size.0.is_some() || new_size.1.is_some() {
            let nvim = nvim.clone();
            let grid = evt.grid;
            let cols = new_size.0.unwrap_or_else(|| grid_metrics.cols) as i64;
            let rows = new_size.1.unwrap_or_else(|| grid_metrics.rows) as i64;
            spawn_local(async move {
                if let Err(err) =
                    nvim.ui_try_resize_grid(grid, cols, rows).await
                {
                    error!("Failed to resize grid({}): {}", grid, err);
                }
            });
        }

        window.set_position(x, y, grid_metrics.width, grid_metrics.height);
        window.show();
    }

    fn window_external_pos(
        &mut self,
        evt: WindowExternalPos,
        window: &gtk::ApplicationWindow,
        nvim: &GioNeovim,
    ) {
        let parent_win = window.clone().upcast::<gtk::Window>();
        let css_provider = self.css_provider.clone();
        let grid = self.grids.get(&evt.grid).unwrap();
        let windows_float_container = self.windows_float_container.clone();
        let window = self.windows.entry(evt.grid).or_insert_with(|| {
            Window::new(
                NvimWindow::new(evt.win.clone(), nvim.clone()),
                windows_float_container,
                &grid,
                Some(css_provider),
                nvim.clone(),
            )
        });

        let grid_metrics = grid.get_grid_metrics();

        window.set_external(
            &parent_win,
            (
                grid_metrics.width.ceil() as i32,
                grid_metrics.height.ceil() as i32,
            ),
        );

        // NOTE(ville): Without this, "new" grids (e.g. once added to a external
        // window without appearing in the main grid first) won't get proper
        // font/linespace values.
        grid.resize(
            &parent_win.get_window().unwrap(),
            grid_metrics.cols as u64,
            grid_metrics.rows as u64,
            &self.hl_defs,
        );
    }

    fn window_hide(&mut self, grid_id: i64) {
        self.windows.get(&grid_id).unwrap().hide();
    }

    fn window_close(&mut self, grid_id: i64) {
        // Drop window.
        if self.windows.remove(&grid_id).is_none() {
            warn!("Nvim instructed to close a window that we don't have (grid: {})", grid_id);
        }
    }

    fn window_viewport(&mut self, e: WindowViewport) {
        if let Some(win) = self.windows.get_mut(&e.grid) {
            let grid = self.grids.get(&e.grid).unwrap();
            let metrics = grid.get_grid_metrics();

            if e.linecount <= metrics.rows as i64 {
                win.hide_scrollbar();
                return;
            }

            win.show_scrollbar();

            let value = metrics.cell_height * e.topline as f64;
            let max = metrics.cell_height * e.linecount as f64;

            win.set_adjustment(
                value,
                0.0,
                max,
                metrics.height,
                metrics.height,
                metrics.height,
                metrics.cell_height,
            );
        }
    }

    fn msg_set_pos(&mut self, e: MsgSetPos) {
        let base_grid = self.grids.get(&1).unwrap();
        let base_metrics = base_grid.get_grid_metrics();
        let grid = self.grids.get(&e.grid).unwrap();
        let h = base_metrics.height - e.row as f64 * base_metrics.cell_height;
        self.msg_window.set_pos(&grid, e.row as f64, h, e.scrolled);
    }

    fn handle_redraw_event(
        &mut self,
        window: &gtk::ApplicationWindow,
        event: RedrawEvent,
        nvim: &GioNeovim,
    ) {
        match event {
            RedrawEvent::SetTitle(evt) => {
                evt.iter().for_each(|e| self.set_title(&window, e));
            }
            RedrawEvent::GridLine(evt) => {
                evt.into_iter().for_each(|line| self.grid_line(line))
            }
            RedrawEvent::GridCursorGoto(evt) => {
                evt.into_iter().for_each(|e| self.grid_cursor_goto(e))
            }
            RedrawEvent::GridResize(evt) => evt
                .into_iter()
                .for_each(|e| self.grid_resize(e, window, nvim)),
            RedrawEvent::GridClear(evt) => {
                evt.iter().for_each(|e| self.grid_clear(e))
            }
            RedrawEvent::GridDestroy(evt) => {
                evt.iter().for_each(|e| self.grid_destroy(e));
            }
            RedrawEvent::GridScroll(evt) => {
                evt.into_iter().for_each(|e| self.grid_scroll(e, nvim))
            }
            RedrawEvent::DefaultColorsSet(evt) => {
                evt.into_iter().for_each(|e| self.default_colors_set(e))
            }
            RedrawEvent::HlAttrDefine(evt) => {
                evt.into_iter().for_each(|e| self.hl_attr_define(e))
            }
            RedrawEvent::HlGroupSet(evt) => {
                evt.into_iter().for_each(|e| self.hl_group_set(e))
            }
            RedrawEvent::OptionSet(evt) => {
                evt.into_iter().for_each(|e| self.option_set(e));
            }
            RedrawEvent::ModeInfoSet(evt) => {
                evt.into_iter().for_each(|e| self.mode_info_set(e));
            }
            RedrawEvent::ModeChange(evt) => {
                evt.into_iter().for_each(|e| self.mode_change(e));
            }
            RedrawEvent::SetBusy(busy) => self.set_busy(busy),
            RedrawEvent::Flush() => self.flush(nvim, window),
            RedrawEvent::PopupmenuShow(evt) => {
                evt.into_iter().for_each(|e| self.popupmenu_show(e));
            }
            RedrawEvent::PopupmenuHide() => self.popupmenu_hide(),
            RedrawEvent::PopupmenuSelect(evt) => {
                evt.into_iter().for_each(|e| self.popupmenu_select(e));
            }
            RedrawEvent::TablineUpdate(evt) => {
                evt.into_iter().for_each(|e| self.tabline_update(e, nvim));
            }
            RedrawEvent::CmdlineShow(evt) => {
                evt.into_iter().for_each(|e| self.cmdline_show(e));
            }
            RedrawEvent::CmdlineHide() => self.cmdline_hide(),
            RedrawEvent::CmdlinePos(evt) => {
                evt.into_iter().for_each(|e| self.cmdline_pos(e));
            }
            RedrawEvent::CmdlineSpecialChar(evt) => {
                evt.into_iter().for_each(|e| self.cmdline_special_char(e));
            }
            RedrawEvent::CmdlineBlockShow(evt) => {
                evt.into_iter().for_each(|e| self.cmdline_block_show(e));
            }
            RedrawEvent::CmdlineBlockAppend(evt) => {
                evt.into_iter().for_each(|e| self.cmdline_block_append(e));
            }
            RedrawEvent::CmdlineBlockHide() => self.cmdline_block_hide(),
            RedrawEvent::WindowPos(evt) => {
                evt.into_iter()
                    .for_each(|e| self.window_pos(e, window, nvim));
            }
            RedrawEvent::WindowFloatPos(evt) => {
                evt.into_iter().for_each(|e| self.window_float_pos(e, nvim));
            }
            RedrawEvent::WindowExternalPos(evt) => {
                evt.into_iter()
                    .for_each(|e| self.window_external_pos(e, window, nvim));
            }
            RedrawEvent::WindowHide(evt) => {
                evt.into_iter().for_each(|e| self.window_hide(e));
            }
            RedrawEvent::WindowClose(evt) => {
                evt.into_iter().for_each(|e| self.window_close(e));
            }
            RedrawEvent::MsgSetPos(evt) => {
                evt.into_iter().for_each(|e| self.msg_set_pos(e));
            }
            RedrawEvent::WindowViewport(evt) => {
                evt.into_iter().for_each(|e| self.window_viewport(e));
            }
            RedrawEvent::Ignored(_) => (),
            RedrawEvent::Unknown(e) => {
                debug!("Received unknown redraw event: {}", e);
            }
        }
    }

    fn handle_gnvim_event(&mut self, event: &GnvimEvent, nvim: &GioNeovim) {
        match event {
            GnvimEvent::CompletionMenuToggleInfo => {
                self.popupmenu.toggle_show_info()
            }
            GnvimEvent::PopupmenuWidth(width) => {
                self.popupmenu.set_width(*width as i32);
            }
            GnvimEvent::PopupmenuWidthDetails(width) => {
                self.popupmenu.set_width_details(*width as i32);
            }
            GnvimEvent::PopupmenuShowMenuOnAllItems(should_show) => {
                self.popupmenu.set_show_menu_on_all_items(*should_show);
            }
            GnvimEvent::Unknown(msg) => {
                debug!("Received unknown GnvimEvent: {}", msg);
            }

            #[cfg(not(feature = "libwebkit2gtk"))]
            GnvimEvent::CursorTooltipLoadStyle(..)
            | GnvimEvent::CursorTooltipShow(..)
            | GnvimEvent::CursorTooltipHide
            | GnvimEvent::CursorTooltipSetStyle(..) => {
                let nvim = nvim.clone();
                let msg =
                    "echom \"Cursor tooltip not supported in this build\"";
                spawn_local(async move {
                    if let Err(err) = nvim.command(&msg).await {
                        error!("Failed to execute nvim command: {}", err)
                    }
                });
            }

            #[cfg(feature = "libwebkit2gtk")]
            GnvimEvent::CursorTooltipLoadStyle(..)
            | GnvimEvent::CursorTooltipShow(..)
            | GnvimEvent::CursorTooltipHide
            | GnvimEvent::CursorTooltipSetStyle(..) => match event {
                GnvimEvent::CursorTooltipLoadStyle(path) => {
                    if let Err(err) =
                        self.cursor_tooltip.load_style(path.clone())
                    {
                        let msg = format!(
                            "echom \"Cursor tooltip load style failed: '{}'\"",
                            err
                        );
                        let nvim = nvim.clone();
                        spawn_local(async move {
                            if let Err(err) = nvim.command(&msg).await {
                                error!(
                                    "Failed to execute nvim command: {}",
                                    err
                                )
                            }
                        });
                    }
                }
                GnvimEvent::CursorTooltipShow(content, row, col) => {
                    self.cursor_tooltip.show(content.clone());

                    let grid = self.grids.get(&self.current_grid).unwrap();
                    let rect = grid.get_rect_for_cell(*row, *col);

                    self.cursor_tooltip.move_to(&rect);
                }
                GnvimEvent::CursorTooltipHide => self.cursor_tooltip.hide(),
                GnvimEvent::CursorTooltipSetStyle(style) => {
                    self.cursor_tooltip.set_style(style)
                }
                _ => unreachable!(),
            },
        }
    }
}

pub fn attach_grid_events(grid: &Grid, nvim: GioNeovim) {
    let id = grid.id;
    // Mouse button press event.
    grid.connect_mouse_button_press_events(
        clone!(nvim => move |button, row, col| {
            let nvim = nvim.clone();
            spawn_local(async move {
                nvim.input_mouse(&button.to_string(), "press", "", id, row as i64, col as i64).await.expect("Couldn't send mouse input");
            });

            Inhibit(false)
        }),
    );

    // Mouse button release events.
    grid.connect_mouse_button_release_events(
        clone!(nvim => move |button, row, col| {
            let nvim = nvim.clone();
            spawn_local(async move {
                nvim.input_mouse(&button.to_string(), "release", "", id, row as i64, col as i64).await.expect("Couldn't send mouse input");
            });

            Inhibit(false)
        }),
    );

    // Mouse drag events.
    grid.connect_motion_events_for_drag(
        clone!(nvim => move |button, row, col| {
            let nvim = nvim.clone();
            spawn_local(async move {
                nvim.input_mouse(&button.to_string(), "drag", "", id, row as i64, col as i64).await.expect("Couldn't send mouse input");
            });

            Inhibit(false)
        }),
    );

    // Scrolling events.
    grid.connect_scroll_events(clone!(nvim => move |dir, row, col| {
        let nvim = nvim.clone();
        spawn_local(async move {
            nvim.input_mouse("wheel", &dir.to_string(), "", id, row as i64, col as i64).await.expect("Couldn't send mouse input");
        });

        Inhibit(false)
    }));
}

fn win_float_adjust_size(
    grid_metrics: &GridMetrics,
    base_metrics: &GridMetrics,
    (x, y): (f64, f64),
) -> (Option<f64>, Option<f64>) {
    let mut new_size = (None, None);
    if grid_metrics.rows + y / base_metrics.cell_height > base_metrics.rows {
        let rows = base_metrics.rows - y / base_metrics.cell_height - 1.0;
        new_size.1 = Some(rows);
    }

    if grid_metrics.cols + x / base_metrics.cell_width > base_metrics.cols {
        let cols = base_metrics.cols - x / base_metrics.cell_width;
        new_size.0 = Some(cols);
    }

    new_size
}

fn win_float_anchor_pos(
    evt: &WindowFloatPos,
    anchor_metrics: &GridMetrics,
    (width, height): (f64, f64),
    (x_offset, y_offset): (f64, f64),
) -> (f64, f64) {
    let x = if evt.anchor.is_west() {
        x_offset + anchor_metrics.cell_width * evt.anchor_col
    } else {
        x_offset + anchor_metrics.cell_width * evt.anchor_col - width
    }
    .max(0.0);

    let y = if evt.anchor.is_north() {
        y_offset + anchor_metrics.cell_height * evt.anchor_row
    } else {
        y_offset + anchor_metrics.cell_height * evt.anchor_row - height
    }
    .max(0.0);

    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvim_bridge::Anchor;
    use rmpv::Value;

    #[test]
    fn test_float_anchor_pos() {
        struct Data {
            anchor: Anchor,
            width: f64,
            height: f64,
            anchor_row: f64,
            anchor_col: f64,
            x_offset: f64,
            y_offset: f64,

            cell_width: f64,
            cell_height: f64,

            expected: (f64, f64),
        }

        let data = vec![
            Data {
                anchor: Anchor::NW,
                width: 1000.0,
                height: 1000.0,
                anchor_row: 10.0,
                anchor_col: 10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (105.0, 105.0),
            },
            Data {
                anchor: Anchor::NW,
                width: 100.0,
                height: 100.0,
                anchor_row: -10.0,
                anchor_col: -10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (0.0, 0.0),
            },
            Data {
                anchor: Anchor::NE,
                width: 100.0,
                height: 100.0,
                anchor_row: 10.0,
                anchor_col: 10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (5.0, 105.0),
            },
            Data {
                anchor: Anchor::SW,
                width: 100.0,
                height: 100.0,
                anchor_row: 10.0,
                anchor_col: 10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (105.0, 5.0),
            },
            Data {
                anchor: Anchor::SW,
                width: 100.0,
                height: 100.0,
                anchor_row: -10.0,
                anchor_col: 10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (105.0, 0.0),
            },
            Data {
                anchor: Anchor::SE,
                width: 100.0,
                height: 100.0,
                anchor_row: 10.0,
                anchor_col: 10.0,
                x_offset: 5.0,
                y_offset: 5.0,

                cell_width: 10.0,
                cell_height: 10.0,

                expected: (5.0, 5.0),
            },
        ];

        for row in data.into_iter() {
            let evt = WindowFloatPos {
                grid: 1,
                win: Value::Nil,
                anchor: row.anchor,
                anchor_grid: 1,
                anchor_row: row.anchor_row,
                anchor_col: row.anchor_col,
                focusable: false,
            };

            assert_eq!(
                row.expected,
                win_float_anchor_pos(
                    &evt,
                    &GridMetrics {
                        cell_height: row.cell_height,
                        cell_width: row.cell_width,
                        rows: 0.0,
                        cols: 0.0,
                        width: 0.0,
                        height: 0.0,
                    },
                    (row.width, row.height),
                    (row.x_offset, row.y_offset),
                ),
            );
        }
    }
}
