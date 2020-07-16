use gtk::prelude::*;

use nvim_rs::Window as NvimWindow;

use crate::nvim_gio::GioWriter;
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

    frame: gtk::Overlay,
    adj: gtk::Adjustment,
    scrollbar: gtk::Scrollbar,

    external_win: Option<gtk::Window>,

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
    ) -> Self {
        let frame = gtk::Overlay::new();
        fixed.put(&frame, 0, 0);

        let widget = grid.widget();
        frame.add(&widget);
        //frame.pack_start(&widget, true, true, 0);

        let adj = gtk::Adjustment::new(0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let scrollbar =
            gtk::Scrollbar::new(gtk::Orientation::Vertical, Some(&adj));
        scrollbar.set_halign(gtk::Align::End);

        // Important to add the css provider for the scrollbar before adding
        // it to the contianer. Otherwise the initial draw will be with the
        // defualt styles and that looks weird.
        if let Some(css_provider) = css_provider {
            add_css_provider!(&css_provider, frame, scrollbar);
        }

        frame.add_overlay(&scrollbar);
        frame.set_overlay_pass_through(&scrollbar, true);
        //frame.pack_end(&scrollbar, false, false, 0);

        Self {
            parent: fixed,
            frame,
            adj,
            scrollbar,
            external_win: None,
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
    ) {
        self.adj.configure(
            value,
            lower,
            upper,
            step_increment,
            page_increment,
            page_size,
        );
    }

    pub fn hide_scrollbar(&self) {
        self.scrollbar.hide();
    }

    pub fn show_scrollbar(&self) {
        self.scrollbar.show();
    }

    pub fn set_parent(&mut self, fixed: gtk::Fixed) {
        if self.parent != fixed {
            self.parent.remove(&self.frame);
            self.parent = fixed;
            self.parent.put(&self.frame, 0, 0);
        }
    }

    pub fn resize(&self, size: (i32, i32)) {
        self.frame.set_size_request(size.0, size.1);
    }

    pub fn set_external(&mut self, parent: &gtk::Window, size: (i32, i32)) {
        if self.external_win.is_some() {
            return;
        }

        self.frame.set_size_request(size.0, size.1);

        let win = gtk::Window::new(gtk::WindowType::Toplevel);
        self.parent.remove(&self.frame);
        win.add(&self.frame);

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
            win.remove(&self.frame);
            self.parent.add(&self.frame);
            win.close();
        }

        self.x = x;
        self.y = y;
        self.parent
            .move_(&self.frame, x.floor() as i32, y.floor() as i32);

        self.frame
            .set_size_request(w.ceil() as i32, h.ceil() as i32);
    }

    pub fn show(&self) {
        self.frame.show_all();
    }

    pub fn hide(&self) {
        self.frame.hide();
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        //if let Some(child) = self.frame.get_child() {
        // We don't want to destroy the child widget, so just remove the child from our
        // container.
        //self.frame.remove(&child);
        //}

        self.parent.remove(&self.frame);

        if let Some(ref win) = self.external_win {
            win.close();
        }
    }
}
