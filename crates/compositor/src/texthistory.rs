//! Undo and redo for a text field bound to a document. Every edit the field sends
//! to `dbd` becomes a change set there, and `undo` of a change set reverts it
//! with a new one. So the field's history is a list of change sets: Command-Z
//! undoes the newest group of them, and redoing undoes those undos.
//!
//! Typing is grouped, as in other editors: characters typed (or deleted) in a run,
//! with no pause of a second or more, undo together. Anything else (a paste, a
//! new line, a cut) is a group of its own.
//!
//! Change sets arrive after the edit that made them (db answers later), so each
//! edit is given its group's id when it's made, and its change set is filed under
//! that id when db answers.

use std::time::{Duration, Instant};

use crate::textedit::Splice;

/// How long a pause ends a run of typing.
const PAUSE: Duration = Duration::from_secs(1);

/// What an edit did, for grouping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Characters typed on one line.
    Typing,
    /// Characters deleted, and nothing inserted.
    Deleting,
    /// Anything else, which is a group of its own.
    Other,
}

impl Kind {
    pub fn of(s: &Splice) -> Kind {
        if s.delete == 0 && !s.insert.is_empty() && !s.insert.contains('\n') && s.insert.chars().count() == 1
        {
            Kind::Typing
        } else if s.insert.is_empty() && s.delete > 0 {
            Kind::Deleting
        } else {
            Kind::Other
        }
    }
}

/// A group of change sets, undone or redone together.
type Group = (u64, Vec<i64>);

#[derive(Debug, Default)]
pub struct TextHistory {
    /// Groups that can be undone, newest last.
    done: Vec<Group>,
    /// Groups of undos, which redo undoes, newest last.
    undone: Vec<Group>,
    /// The group still taking edits: its kind and when it last took one.
    open: Option<(Kind, Instant)>,
    next: u64,
}

impl TextHistory {
    /// The field made an edit of `kind` at `now`: the id of the group its change
    /// set belongs in. A new edit can't be redone over, so redo is cleared.
    pub fn edit(&mut self, kind: Kind, now: Instant) -> u64 {
        self.undone.clear();
        let continues = kind != Kind::Other
            && self.open.is_some_and(|(k, at)| k == kind && now.saturating_duration_since(at) < PAUSE);
        self.open = Some((kind, now));
        match self.done.last() {
            Some(&(id, _)) if continues => id,
            _ => self.start(false),
        }
    }

    /// Change sets made elsewhere (by the field's app) to undo as one step, after
    /// the field's own edits so far. Like an edit, it clears redo.
    pub fn add(&mut self, sets: Vec<i64>) {
        if sets.is_empty() {
            return;
        }
        self.undone.clear();
        self.open = None;
        let id = self.start(false);
        self.done.last_mut().expect("just started").1 = sets;
        debug_assert_eq!(self.done.last().map(|g| g.0), Some(id));
    }

    /// db answered an edit (or an undo) of group `group` with change set `set`.
    pub fn record(&mut self, group: u64, set: i64) {
        if let Some((_, sets)) =
            self.done.iter_mut().chain(self.undone.iter_mut()).find(|(id, _)| *id == group)
        {
            sets.push(set);
        }
    }

    /// db refused an edit (or undo) of `group`: if nothing of it was kept, it's
    /// no step at all.
    pub fn forget(&mut self, group: u64) {
        for stack in [&mut self.done, &mut self.undone] {
            stack.retain(|(id, sets)| *id != group || !sets.is_empty());
        }
    }

    /// The change sets to undo, newest first, and the group their undos go in
    /// (which redo will undo). None when there's nothing to undo.
    pub fn undo(&mut self) -> Option<(Vec<i64>, u64)> {
        self.open = None;
        let sets = pop_filled(&mut self.done)?;
        Some((sets.into_iter().rev().collect(), self.start(true)))
    }

    /// The undos to undo, newest first, and the group those go in.
    pub fn redo(&mut self) -> Option<(Vec<i64>, u64)> {
        self.open = None;
        let sets = pop_filled(&mut self.undone)?;
        Some((sets.into_iter().rev().collect(), self.start(false)))
    }

    /// A new, empty group at the top of `done` (or `undone`): its id.
    fn start(&mut self, undone: bool) -> u64 {
        self.next += 1;
        let stack = if undone { &mut self.undone } else { &mut self.done };
        stack.push((self.next, Vec::new()));
        self.next
    }
}

/// The newest group in `stack`, unless db hasn't answered for it yet (it has no
/// change sets): then nothing, until it has.
fn pop_filled(stack: &mut Vec<Group>) -> Option<Vec<i64>> {
    if stack.last()?.1.is_empty() {
        return None;
    }
    stack.pop().map(|(_, sets)| sets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(c: &str) -> Kind {
        Kind::of(&Splice { at: 0, delete: 0, insert: c.into() })
    }

    #[test]
    fn edits_are_sorted_into_kinds() {
        assert_eq!(typed("a"), Kind::Typing);
        assert_eq!(typed("\n"), Kind::Other);
        assert_eq!(typed("pasted"), Kind::Other);
        assert_eq!(Kind::of(&Splice { at: 3, delete: 1, insert: "".into() }), Kind::Deleting);
        assert_eq!(Kind::of(&Splice { at: 3, delete: 2, insert: "x".into() }), Kind::Other);
    }

    #[test]
    fn a_run_of_typing_undoes_together_and_a_pause_ends_it() {
        let mut h = TextHistory::default();
        let t = Instant::now();
        let a = h.edit(Kind::Typing, t);
        let b = h.edit(Kind::Typing, t + Duration::from_millis(300));
        assert_eq!(a, b, "one run");
        let c = h.edit(Kind::Typing, t + Duration::from_millis(1500));
        assert_ne!(b, c, "a pause starts another");
        let d = h.edit(Kind::Deleting, t + Duration::from_millis(1600));
        assert_ne!(c, d, "deleting isn't typing");
        // db answers late, out of step with the edits.
        h.record(a, 1);
        h.record(a, 2);
        h.record(c, 3);
        h.record(d, 4);
        assert_eq!(h.undo().map(|(s, _)| s), Some(vec![4]));
        assert_eq!(h.undo().map(|(s, _)| s), Some(vec![3]));
        let (sets, undos) = h.undo().unwrap();
        assert_eq!(sets, [2, 1], "newest first");
        assert_eq!(h.undo(), None);
        // Redo undoes the undos.
        h.record(undos, 10);
        h.record(undos, 11);
        let (sets, _) = h.redo().unwrap();
        assert_eq!(sets, [11, 10]);
        assert_eq!(h.redo(), None);
    }

    #[test]
    fn a_new_edit_clears_redo_and_other_edits_stand_alone() {
        let mut h = TextHistory::default();
        let t = Instant::now();
        let a = h.edit(Kind::Other, t);
        let b = h.edit(Kind::Other, t);
        assert_ne!(a, b);
        h.record(a, 4);
        h.record(b, 5);
        let (_, undos) = h.undo().unwrap();
        h.record(undos, 6);
        h.edit(Kind::Typing, t);
        assert_eq!(h.redo(), None, "typing after an undo drops the redo");
        // After an undo, typing starts a new group even within the pause.
        let mut h = TextHistory::default();
        let a = h.edit(Kind::Typing, t);
        h.record(a, 1);
        h.undo();
        let b = h.edit(Kind::Typing, t);
        assert_ne!(a, b);
        // An app's own change sets undo as one step, after the field's.
        let mut h = TextHistory::default();
        let a = h.edit(Kind::Typing, t);
        h.record(a, 1);
        h.add(vec![7, 8]);
        assert_eq!(h.undo().map(|(s, _)| s), Some(vec![8, 7]));
        assert_eq!(h.undo().map(|(s, _)| s), Some(vec![1]));
        // A group db hasn't answered for yet waits; one it refused is dropped.
        let mut h = TextHistory::default();
        let a = h.edit(Kind::Other, t);
        h.record(a, 1);
        let b = h.edit(Kind::Other, t);
        assert_eq!(h.undo(), None, "waits for b's change set");
        h.forget(b);
        assert_eq!(h.undo().map(|(s, _)| s), Some(vec![1]));
    }
}
