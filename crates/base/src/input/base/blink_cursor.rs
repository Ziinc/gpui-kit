use gpui::{App, Context, Global, Pixels, Task, px};
use instant::Duration;

static INTERVAL: Duration = Duration::from_millis(500);
static PAUSE_DELAY: Duration = Duration::from_millis(300);

pub(super) const CURSOR_WIDTH: Pixels = px(1.);

/// Application-wide caret blinking for text inputs.
///
/// Every blink repaints the window, so a slower blink, or none, saves work
/// when frames are costly, such as under software rendering. Read on every
/// blink, so installing new defaults takes effect in inputs that are already
/// focused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorBlinkDefaults {
    interval: Option<Duration>,
}

impl Global for CursorBlinkDefaults {}

impl CursorBlinkDefaults {
    /// Creates the Base defaults: the caret shows and hides every 500 ms.
    pub fn new() -> Self {
        Self {
            interval: Some(INTERVAL),
        }
    }

    /// Sets how long the caret stays shown, then hidden. `None` keeps it
    /// shown without blinking.
    pub fn with_interval(mut self, interval: impl Into<Option<Duration>>) -> Self {
        self.interval = interval.into();
        self
    }

    /// How long the caret stays shown, then hidden; `None` when it does not blink.
    pub fn interval(&self) -> Option<Duration> {
        self.interval
    }

    /// Installs these defaults for the whole application.
    pub fn install(self, cx: &mut App) {
        cx.set_global(self);
    }

    /// Returns the installed defaults, or the Base ones when none were.
    pub fn global(cx: &App) -> Self {
        cx.try_global::<Self>().copied().unwrap_or_default()
    }
}

impl Default for CursorBlinkDefaults {
    fn default() -> Self {
        Self::new()
    }
}

/// To manage the Input cursor blinking.
///
/// It will start blinking with the interval of [`CursorBlinkDefaults`].
/// Every loop will notify the view to update the `visible`, and Input will observe this update to touch repaint.
///
/// The input painter will check if this in visible state, then it will draw the cursor.
pub(crate) struct BlinkCursor {
    visible: bool,
    paused: bool,
    epoch: usize,

    _task: Task<()>,
}

impl BlinkCursor {
    pub(crate) fn new() -> Self {
        Self {
            visible: false,
            paused: false,
            epoch: 0,
            _task: Task::ready(()),
        }
    }

    /// Start the blinking from a visible cursor.
    ///
    /// A single focus can start the cursor more than once: `InputState::focus`
    /// starts it and then the focus observer starts it again, and window
    /// activation starts it too. Resuming the running cycle would toggle the
    /// cursor that was just shown off again, so every start restarts the cycle.
    pub(crate) fn start(&mut self, cx: &mut Context<Self>) {
        self.paused = false;
        self.visible = false;
        let epoch = self.next_epoch();
        self.blink(epoch, cx);
    }

    /// Stop the blinking and clear the blink state, so the next [`Self::start`]
    /// begins from a visible cursor instead of resuming a stale pause.
    pub(crate) fn stop(&mut self, cx: &mut Context<Self>) {
        self.epoch = 0;
        self.paused = false;
        self.visible = false;
        self._task = Task::ready(());
        cx.notify();
    }

    fn next_epoch(&mut self) -> usize {
        self.epoch += 1;
        self.epoch
    }

    fn blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        // A task from an earlier blink lifecycle must not mutate the current state.
        if epoch != self.epoch {
            return;
        }

        if self.paused {
            self.visible = true;
            return;
        }

        let Some(interval) = CursorBlinkDefaults::global(cx).interval() else {
            // Not blinking: a steady caret, and no timer to repaint it.
            self.visible = true;
            cx.notify();
            return;
        };

        self.visible = !self.visible;
        cx.notify();

        // Schedule the next blink
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(interval).await;
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.blink(epoch, cx));
            }
        });
    }

    pub(crate) fn visible(&self) -> bool {
        // Keep showing the cursor if paused
        self.paused || self.visible
    }

    /// Show the cursor immediately and restart the idle delay before blinking resumes.
    ///
    /// This is a no-op on a cursor that is not blinking. `epoch` is zero only
    /// before the first [`Self::start`] and after [`Self::stop`], so a zero
    /// epoch means the input is not focused, and there is no cursor on screen
    /// to keep visible. Pausing it anyway would start a blink loop that no blur
    /// is left to stop, and every blink repaints the view that the input is in.
    pub(crate) fn pause(&mut self, cx: &mut Context<Self>) {
        if self.epoch == 0 {
            return;
        }

        self.paused = true;
        self.visible = true;
        cx.notify();

        // Every pause replaces the pending timer, keeping repeated input visible.
        let epoch = self.next_epoch();
        self._task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(PAUSE_DELAY).await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| {
                    if epoch != this.epoch {
                        return;
                    }

                    this.paused = false;
                    this.blink(epoch, cx);
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use std::{cell::Cell, rc::Rc};

    #[gpui::test]
    fn repeated_pauses_keep_cursor_visible_until_idle(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        // Only a focused input blinks, and only a blinking cursor pauses.
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        for _ in 0..5 {
            cursor.update(cx, |cursor, cx| cursor.pause(cx));
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(200));
            cx.run_until_parked();
            assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        }
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        cx.executor().advance_clock(INTERVAL);
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    #[gpui::test]
    fn the_installed_interval_paces_the_blink(cx: &mut TestAppContext) {
        let interval = Duration::from_secs(1);
        cx.update(|cx| {
            CursorBlinkDefaults::new()
                .with_interval(interval)
                .install(cx)
        });
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        cx.executor()
            .advance_clock(interval - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    #[gpui::test]
    fn without_an_interval_the_cursor_stays_visible_and_idle(cx: &mut TestAppContext) {
        cx.update(|cx| CursorBlinkDefaults::new().with_interval(None).install(cx));
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));

        let notifies = Rc::new(Cell::new(0usize));
        let counter = notifies.clone();
        let _observer =
            cx.update(|cx| cx.observe(&cursor, move |_, _| counter.set(counter.get() + 1)));
        cx.run_until_parked();
        // Typing pauses and resumes the cursor; it must not start blinking.
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.executor().advance_clock(INTERVAL * 6);
        cx.run_until_parked();

        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        assert!(
            notifies.get() <= 2,
            "a cursor without an interval kept blinking"
        );
    }

    #[gpui::test]
    fn starting_a_blinking_cursor_keeps_it_visible(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());

        // Clicking into an input starts the cursor from `focus` and again from
        // the focus observer; the second start must not hide the new cursor.
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));

        // The restart drops the first cycle's timer, so the cursor stays
        // visible for a full interval before it blinks off.
        cx.executor()
            .advance_clock(INTERVAL - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    #[gpui::test]
    fn pausing_a_cursor_that_is_not_blinking_does_not_start_it(cx: &mut TestAppContext) {
        // Never focused, so `start` was never called: the epoch is still zero.
        let cursor = cx.new(|_| BlinkCursor::new());

        // What a programmatic `set_value` on an unfocused input does.
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        cx.executor().advance_clock(PAUSE_DELAY);
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));

        let notifies = Rc::new(Cell::new(0usize));
        let counter = notifies.clone();
        let _observer =
            cx.update(|cx| cx.observe(&cursor, move |_, _| counter.set(counter.get() + 1)));
        cx.run_until_parked();

        cx.executor().advance_clock(INTERVAL * 6);
        cx.run_until_parked();

        assert_eq!(
            notifies.get(),
            0,
            "a cursor that was never started is blinking, and every blink repaints the view"
        );
    }

    #[gpui::test]
    fn blurring_a_paused_cursor_leaves_the_next_focus_blinking(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();

        // Typing pauses the blink, then the input is blurred before the pause
        // elapses: tabbing away right after a keystroke does exactly this.
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        cursor.update(cx, |cursor, cx| cursor.stop(cx));
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));

        // Focusing again shows the cursor and blinks it, rather than leaving a
        // stale pause to swallow the start.
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
        cx.executor().advance_clock(INTERVAL);
        cx.run_until_parked();
        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    #[gpui::test]
    fn stopping_a_paused_cursor_ends_the_blink_loop(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        cursor.update(cx, |cursor, cx| cursor.pause(cx));
        cx.run_until_parked();
        cursor.update(cx, |cursor, cx| cursor.stop(cx));
        cx.run_until_parked();

        let notifications = Rc::new(Cell::new(0usize));
        let count = notifications.clone();
        let _observer = cx.update(|cx| cx.observe(&cursor, move |_, _| count.set(count.get() + 1)));
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();

        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        assert_eq!(notifications.get(), 0, "a stopped cursor kept blinking");

        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        assert!(cursor.read_with(cx, |cursor, _| cursor.visible()));
    }

    #[gpui::test]
    fn stopping_a_blinking_cursor_ends_the_blink_loop(cx: &mut TestAppContext) {
        let cursor = cx.new(|_| BlinkCursor::new());
        cursor.update(cx, |cursor, cx| cursor.start(cx));
        cx.run_until_parked();
        cursor.update(cx, |cursor, cx| cursor.stop(cx));
        cx.run_until_parked();

        let notifications = Rc::new(Cell::new(0usize));
        let count = notifications.clone();
        let _observer = cx.update(|cx| cx.observe(&cursor, move |_, _| count.set(count.get() + 1)));
        cx.run_until_parked();

        cx.executor().advance_clock(Duration::from_secs(3));
        cx.run_until_parked();

        assert!(!cursor.read_with(cx, |cursor, _| cursor.visible()));
        assert_eq!(notifications.get(), 0, "a stopped cursor kept blinking");
    }
}
