//! The text around the cursor of the focused field, read through AT-SPI.
//!
//! Right before an insertion, [`FocusedField::context`] looks for the active
//! window on the accessibility bus, finds the focused object in it and reads
//! that object's selection and the text around it. A focused object that is
//! not editable, TypeWhisper's own window, or an app that does not answer in
//! time gives no context, and the dictation is then inserted unchanged.
//!
//! The search asks the apps for their objects instead of listening for focus
//! events: registering an event listener switches on the desktop's
//! accessibility support (`org.a11y.Status.IsEnabled`) for every app, which
//! TypeWhisper leaves to the user. GTK apps publish their objects anyway. Qt,
//! Firefox and Chromium do so only while accessibility is on, for example while
//! a screen reader runs. Inside Flatpak, the accessibility bus is filtered so
//! that an app sees no other app, so there is no context there.

use std::future::Future;
use std::ops::Range;
use std::time::Duration;

use ashpd::zbus;
use async_io::Timer;
use futures_util::{future, pin_mut};

const DESKTOP: &str = "org.a11y.atspi.Registry";
const DESKTOP_PATH: &str = "/org/a11y/atspi/accessible/root";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const TEXT: &str = "org.a11y.atspi.Text";
/// Longest wait for finding the focused field and reading its text.
const READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Longest wait for one app to list its windows, so that an app that hangs
/// does not hide the active window of another.
const APP_TIMEOUT: Duration = Duration::from_millis(150);
/// Objects searched for the focus at most, so that a huge window cannot stall
/// the insertion.
const SEARCH_LIMIT: usize = 3000;
/// Characters read on each side of the selection. The rules only look at the
/// nearest characters that are not whitespace.
const WINDOW: i32 = 512;
/// Bits of `AtspiStateType`.
const STATE_ACTIVE: u32 = 1;
const STATE_EDITABLE: u32 = 7;
const STATE_FOCUSED: u32 = 12;
const STATE_SHOWING: u32 = 25;

/// The text of the focused field around its selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InsertionContext {
    /// The field's text, or the part of it around the selection.
    pub value: String,
    /// The selection in characters of `value`; the caret is an empty range.
    pub selection: Range<usize>,
}

/// An accessible object: the app's name on the bus and the object's path.
type Object = (String, zbus::OwnedObjectPath);

pub struct FocusedField {
    connection: zbus::Connection,
}

impl FocusedField {
    /// Connects to the accessibility bus.
    pub async fn connect(session: &zbus::Connection) -> zbus::Result<Self> {
        let address = bus_address(session).await?;
        let connection = zbus::connection::Builder::address(address.as_str())
            .build()
            .await?;
        Ok(Self { connection })
    }

    /// Reads the selection and the text around it from the focused field.
    pub async fn context(&self) -> Option<InsertionContext> {
        within(READ_TIMEOUT, async {
            let field = focused_field(&self.connection).await?;
            read_context(&self.connection, &field).await
        })
        .await
    }
}

/// Sessions and Flatpak set `AT_SPI_BUS_ADDRESS`; otherwise the session bus knows it.
async fn bus_address(session: &zbus::Connection) -> zbus::Result<String> {
    if let Some(address) = std::env::var("AT_SPI_BUS_ADDRESS")
        .ok()
        .filter(|a| !a.is_empty())
    {
        return Ok(address);
    }
    session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .await?
        .body()
        .deserialize()
}

/// The focused editable object in the active window. `None` while one of
/// TypeWhisper's own windows is active, since the paste then goes there.
async fn focused_field(connection: &zbus::Connection) -> Option<Object> {
    let desktop = (DESKTOP.to_owned(), DESKTOP_PATH.try_into().ok()?);
    let apps: Vec<Object> = call(connection, &desktop, ACCESSIBLE, "GetChildren", &())
        .await
        .ok()?;
    let windows = future::join_all(apps.iter().map(|app| active_windows(connection, app))).await;
    let own = std::process::id();
    // Should several windows claim to be active, the first with a focused field wins.
    for window in windows.into_iter().flatten() {
        if process_id(connection, &window.0).await == Some(own) {
            return None;
        }
        if let Some(field) = focused_field_in(connection, window).await {
            return Some(field);
        }
    }
    None
}

/// The app's windows that have the keyboard focus, usually one or none.
async fn active_windows(connection: &zbus::Connection, app: &Object) -> Vec<Object> {
    within(APP_TIMEOUT, async {
        let windows = children(connection, app).await;
        let states = future::join_all(windows.iter().map(|w| states(connection, w))).await;
        Some(
            windows
                .into_iter()
                .zip(states)
                .filter(|(_, states)| {
                    states
                        .as_deref()
                        .is_some_and(|s| has_state(s, STATE_ACTIVE))
                })
                .map(|(window, _)| window)
                .collect(),
        )
    })
    .await
    .unwrap_or_default()
}

/// Searches the window level by level for a focused, editable object, asking
/// for all objects of a level at once. Hidden objects are skipped with their
/// children. Qt marks the containers around the focus as focused too, so a
/// focused object that is not editable does not end the search.
async fn focused_field_in(connection: &zbus::Connection, window: Object) -> Option<Object> {
    let mut level = vec![window];
    let mut searched = 0;
    while !level.is_empty() && searched < SEARCH_LIMIT {
        searched += level.len();
        let states = future::join_all(level.iter().map(|o| states(connection, o))).await;
        let mut showing = Vec::new();
        for (object, states) in level.into_iter().zip(states) {
            let Some(states) = states else { continue };
            if has_state(&states, STATE_FOCUSED) && has_state(&states, STATE_EDITABLE) {
                return Some(object);
            }
            if has_state(&states, STATE_SHOWING) {
                showing.push(object);
            }
        }
        let children = future::join_all(showing.iter().map(|o| children(connection, o))).await;
        level = children.into_iter().flatten().collect();
    }
    None
}

async fn children(connection: &zbus::Connection, object: &Object) -> Vec<Object> {
    call(connection, object, ACCESSIBLE, "GetChildren", &())
        .await
        .unwrap_or_default()
}

async fn states(connection: &zbus::Connection, object: &Object) -> Option<Vec<u32>> {
    call(connection, object, ACCESSIBLE, "GetState", &())
        .await
        .ok()
}

async fn process_id(connection: &zbus::Connection, bus_name: &str) -> Option<u32> {
    connection
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "GetConnectionUnixProcessID",
            &(bus_name,),
        )
        .await
        .ok()?
        .body()
        .deserialize()
        .ok()
}

async fn within<T>(duration: Duration, work: impl Future<Output = Option<T>>) -> Option<T> {
    pin_mut!(work);
    match future::select(work, Timer::after(duration)).await {
        future::Either::Left((value, _)) => value,
        future::Either::Right(_) => None,
    }
}

async fn read_context(connection: &zbus::Connection, target: &Object) -> Option<InsertionContext> {
    let (caret, count, selections) = future::try_join3(
        text_property(connection, target, "CaretOffset"),
        text_property(connection, target, "CharacterCount"),
        call::<i32>(connection, target, TEXT, "GetNSelections", &()),
    )
    .await
    .ok()?;
    let (start, end) = if selections > 0 {
        let (start, end): (i32, i32) = call(connection, target, TEXT, "GetSelection", &(0i32,))
            .await
            .ok()?;
        (start.min(end), start.max(end))
    } else {
        (caret, caret)
    };
    if start < 0 || end > count {
        return None;
    }
    // Only the text around the selection matters, so a large selection is not
    // read: the two windows are joined, and the selection becomes the point
    // between them.
    let from = (start - WINDOW).max(0);
    let to = end.saturating_add(WINDOW).min(count);
    let (before, after): (String, String) = future::try_join(
        call(connection, target, TEXT, "GetText", &(from, start)),
        call(connection, target, TEXT, "GetText", &(end, to)),
    )
    .await
    .ok()?;
    let point = before.chars().count();
    window_context(before + &after, point..point, from > 0, to < count)
}

/// Builds the context from text read around the selection. When the text
/// between the selection and a cut-off edge is all whitespace, the nearest
/// other character lies beyond the edge and is unknown, so there is no context.
fn window_context(
    value: String,
    selection: Range<usize>,
    cut_before: bool,
    cut_after: bool,
) -> Option<InsertionContext> {
    let characters = value.chars().count();
    if selection.start > selection.end || selection.end > characters {
        return None;
    }
    let mut before = value.chars().take(selection.start);
    let mut after = value.chars().skip(selection.end);
    if (cut_before && before.all(char::is_whitespace))
        || (cut_after && after.all(char::is_whitespace))
    {
        return None;
    }
    Some(InsertionContext { value, selection })
}

fn has_state(states: &[u32], state: u32) -> bool {
    states
        .get((state / 32) as usize)
        .is_some_and(|word| word & (1 << (state % 32)) != 0)
}

async fn text_property(
    connection: &zbus::Connection,
    target: &Object,
    name: &str,
) -> zbus::Result<i32> {
    let value: zbus::OwnedVariant = call(
        connection,
        target,
        "org.freedesktop.DBus.Properties",
        "Get",
        &(TEXT, name),
    )
    .await?;
    i32::try_from(value)
}

async fn call<R>(
    connection: &zbus::Connection,
    target: &Object,
    interface: &str,
    method: &str,
    body: &(impl zbus::export::serde::Serialize + zbus::DynamicType),
) -> zbus::Result<R>
where
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
{
    connection
        .call_method(
            Some(target.0.as_str()),
            target.1.as_str(),
            Some(interface),
            method,
            body,
        )
        .await?
        .body()
        .deserialize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_state_bits_across_both_words() {
        let states = [1 << STATE_FOCUSED | 1 << STATE_EDITABLE, 1 << (38 - 32)];
        assert!(has_state(&states, STATE_FOCUSED));
        assert!(has_state(&states, STATE_EDITABLE));
        assert!(has_state(&states, 38));
        assert!(!has_state(&states, 1));
        assert!(!has_state(&[], STATE_FOCUSED));
    }

    #[test]
    fn keeps_a_window_with_text_on_both_sides() {
        let context = window_context("über das".into(), 4..4, true, true).unwrap();
        assert_eq!(context.selection, 4..4);
    }

    #[test]
    fn rejects_a_window_that_hides_the_nearest_character() {
        assert_eq!(window_context("   word".into(), 2..2, true, false), None);
        assert_eq!(window_context("word   ".into(), 5..5, false, true), None);
        // At the start or end of the field, whitespace is all there is.
        assert!(window_context("   word".into(), 2..2, false, false).is_some());
    }

    #[test]
    fn rejects_a_selection_outside_the_text() {
        assert_eq!(window_context("ab".into(), 1..3, false, false), None);
        let reversed = Range { start: 2, end: 1 };
        assert_eq!(window_context("ab".into(), reversed, false, false), None);
    }
}
