use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use nvim_rs::Window as NvimWindow;

use crate::nvim_gio::{GioNeovim, GioWriter};
use crate::ui::common::spawn_local;
use crate::ui::grid::Grid;

pub struct MsgWindow {
    fixed: gtk::Fixed,
    frame: gtk::Frame,
}

impl MsgWindow {
    pub fn new(fixed: gtk::Fixed, css_provider: gtk::CssProvider) -> Self {
        let frame = gtk::Frame::new(None);

        fixed.put(&frame, 0, 0);

        add_css_provider!(&css_provider, frame);

        Self { fixed, frame }
    }

    /// Set the position of the message window.
    ///
    /// * `grid` - The grid to set to the window.
    /// * `row` - The row on the parent window where the message window should
    ///           start. The position in pixels is calculated based on the `grid`.
    /// * `h` - Height of the window. While we can calculate the position based
    ///         on the `grid` and `row`, we can't calculate the height automatically.
    ///         The height is mainly needed so we don't show any artifacts that
    ///         will likely be visible on the `grid`'s drawingarea from earlier renders.
    pub fn set_pos(&self, grid: &Grid, row: f64, h: f64, scrolled: bool) {
        let w = grid.widget();

        // Only add/change the child widget if its different
        // from the previous one.
        if let Some(child) = self.frame.get_child() {
            if w != child {
                self.frame.remove(&child);
                w.unparent(); // Unparent the grid.
                self.frame.add(&w);
            }
        } else {
            self.frame.add(&w);
        }

        let c = self.frame.get_style_context();
        if scrolled {
            c.add_class("scrolled");
        } else {
            c.remove_class("scrolled");
        }

        let metrics = grid.get_grid_metrics();
        let w = metrics.cols * metrics.cell_width;
        self.frame
            .set_size_request(w.ceil() as i32, h.ceil() as i32);

        self.fixed.move_(
            &self.frame,
            0,
            (metrics.cell_height as f64 * row) as i32,
        );
        self.fixed.show_all();
    }
}

pub struct Window {
    parent: gtk::Fixed,

    overlay: gtk::Overlay,
    adj: gtk::Adjustment,
    scrollbar: gtk::Scrollbar,

    external_win: Option<gtk::Window>,
    nvim: GioNeovim,
    adj_changed_signal_id: glib::SignalHandlerId,

    last_value: Rc<RefCell<f64>>,
    cell_height: Rc<RefCell<f64>>,

    pub x: f64,
    pub y: f64,

    /// Currently shown grid's id.
    pub grid_id: i64,
    pub nvim_win: NvimWindow<GioWriter>,
}

impl Window {
    pub fn new(
        win: NvimWindow<GioWriter>,
        fixed: gtk::Fixed,
        grid: &Grid,
        css_provider: Option<gtk::CssProvider>,
        nvim: GioNeovim,
    ) -> Self {
        let overlay = gtk::Overlay::new();
        fixed.put(&overlay, 0, 0);

        let widget = grid.widget();
        overlay.add(&widget);

        let last_value = Rc::new(RefCell::new(0.0));
        let cell_height = Rc::new(RefCell::new(0.0));
        let adj = gtk::Adjustment::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let adj_changed_signal_id =
            adj.connect_value_changed(clone!(nvim, last_value, cell_height => move |adj| {
                let nvim = nvim.clone();
                let cell_height = *cell_height.borrow();
                let last_value = *last_value.borrow() / cell_height;

                // TODO(ville): Spamming the input to nvim doesn't scale well on big documents.
                // Find another way.
                let d = (last_value - adj.get_value() / cell_height).ceil();
                let op = if d < 0.0 {
                    "<C-e>"
                } else {
                    "<C-y>"
                };
                let cmd = format!("{}", op.repeat(d.abs() as usize));

                // TODO(ville): "Block" on this.
                spawn_local(async move {
                    nvim.input(&cmd).await.unwrap();
                });
            }));

        let scrollbar =
            gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&adj));
        scrollbar.set_halign(gtk::Align::End);

        // Important to add the css provider for the scrollbar before adding
        // it to the contianer. Otherwise the initial draw will be with the
        // defualt styles and that looks weird.
        if let Some(css_provider) = css_provider {
            add_css_provider!(&css_provider, overlay, scrollbar);
        }

        overlay.add_overlay(&scrollbar);
        overlay.set_overlay_pass_through(&scrollbar, true);

        Self {
            parent: fixed,
            overlay,
            adj,
            scrollbar,
            external_win: None,
            nvim,
            last_value,
            cell_height,
            adj_changed_signal_id,
            grid_id: grid.id,
            nvim_win: win,
            x: 0.0,
            y: 0.0,
        }
    }

    pub fn set_adjustment(
        &mut self,
        value: f64,
        lower: f64,
        upper: f64,
        step_increment: f64,
        page_increment: f64,
        page_size: f64,
        cell_height: f64,
    ) {
        glib::signal_handler_block(&self.adj, &self.adj_changed_signal_id);

        self.adj.configure(
            value,
            lower,
            upper,
            step_increment,
            page_increment,
            page_size,
        );

        *self.last_value.borrow_mut() = value;
        *self.cell_height.borrow_mut() = cell_height;

        glib::signal_handler_unblock(&self.adj, &self.adj_changed_signal_id);
    }

    pub fn hide_scrollbar(&self) {
        self.scrollbar.hide();
    }

    pub fn show_scrollbar(&self) {
        self.scrollbar.show();
    }

    pub fn set_parent(&mut self, fixed: gtk::Fixed) {
        if self.parent != fixed {
            self.parent.remove(&self.overlay);
            self.parent = fixed;
            self.parent.put(&self.overlay, 0, 0);
        }
    }

    pub fn resize(&self, size: (i32, i32)) {
        self.overlay.set_size_request(size.0, size.1);
    }

    pub fn set_external(&mut self, parent: &gtk::Window, size: (i32, i32)) {
        if self.external_win.is_some() {
            return;
        }

        self.overlay.set_size_request(size.0, size.1);

        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        self.parent.remove(&self.overlay);
        win.add(&self.overlay);

        win.set_accept_focus(false);
        win.set_deletable(false);
        win.set_resizable(false);

        win.set_transient_for(Some(parent));
        win.set_attached_to(Some(parent));

        win.show_all();

        self.external_win = Some(win);
    }

    pub fn set_position(&mut self, x: f64, y: f64, w: f64, h: f64) {
        if let Some(win) = self.external_win.take() {
            win.remove(&self.overlay);
            self.parent.add(&self.overlay);
            win.close();
        }

        self.x = x;
        self.y = y;
        self.parent
            .move_(&self.overlay, x.floor() as i32, y.floor() as i32);

        self.overlay
            .set_size_request(w.ceil() as i32, h.ceil() as i32);
    }

    pub fn show(&self) {
        self.overlay.show_all();
    }

    pub fn hide(&self) {
        self.overlay.hide();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        if let Some(child) = self.overlay.get_child() {
            // We don't want to destroy the child widget, so just remove the child from our
            // container.
            self.overlay.remove(&child);
        }

        self.parent.remove(&self.overlay);

        if let Some(ref win) = self.external_win {
            win.close();
        }
    }
}
