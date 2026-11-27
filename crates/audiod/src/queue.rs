//! The play queue: tracks in play order, shuffled or not, and what comes next.
//!
//! Entries have keys that stay the same while the queue changes around them, so
//! the decoder and the queue can talk about "the track after this one" while
//! entries are added, removed or shuffled.

use protocol::audio::{QueueItem, Repeat};

pub type Key = u64;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub key: Key,
    pub item: QueueItem,
    /// The blob hash of its audio.
    pub audio: String,
}

#[derive(Default)]
pub struct Queue {
    entries: Vec<Entry>,
    /// The order before shuffling, to put back; `None` when not shuffled.
    unshuffled: Option<Vec<Key>>,
    next_key: Key,
    rng: u64,
}

/// Which way to step through the queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrap {
    /// Nothing after the last.
    No,
    /// After the last, the first.
    Yes,
}

impl Queue {
    pub fn new(seed: u64) -> Queue {
        Queue { rng: seed | 1, ..Queue::default() }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn shuffled(&self) -> bool {
        self.unshuffled.is_some()
    }

    pub fn index(&self, key: Key) -> Option<usize> {
        self.entries.iter().position(|e| e.key == key)
    }

    pub fn get(&self, key: Key) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key == key)
    }

    pub fn key_at(&self, index: usize) -> Option<Key> {
        self.entries.get(index).map(|e| e.key)
    }

    /// Replace everything with `items`; their keys, in order.
    pub fn replace(&mut self, items: Vec<(QueueItem, String)>) -> Vec<Key> {
        self.entries.clear();
        self.unshuffled = None;
        self.add(items, self.entries.len())
    }

    /// Add `items` at the end, or right after `after` (the current track).
    pub fn enqueue(&mut self, items: Vec<(QueueItem, String)>, after: Option<Key>) -> Vec<Key> {
        let at = after.and_then(|k| self.index(k)).map_or(self.entries.len(), |i| i + 1);
        let keys = self.add(items, at);
        if let Some(order) = &mut self.unshuffled {
            let at = after.and_then(|k| order.iter().position(|o| *o == k)).map_or(order.len(), |i| i + 1);
            order.splice(at..at, keys.iter().copied());
        }
        keys
    }

    fn add(&mut self, items: Vec<(QueueItem, String)>, at: usize) -> Vec<Key> {
        let entries: Vec<Entry> = items
            .into_iter()
            .map(|(item, audio)| {
                self.next_key += 1;
                Entry { key: self.next_key, item, audio }
            })
            .collect();
        let keys = entries.iter().map(|e| e.key).collect();
        self.entries.splice(at..at, entries);
        keys
    }

    pub fn remove(&mut self, key: Key) {
        self.entries.retain(|e| e.key != key);
        if let Some(order) = &mut self.unshuffled {
            order.retain(|k| *k != key);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.unshuffled = None;
    }

    /// Shuffle what comes after `current` (all of it with no current track), or
    /// put the original order back.
    pub fn set_shuffle(&mut self, on: bool, current: Option<Key>) {
        match (on, self.unshuffled.is_some()) {
            (true, false) => {
                self.unshuffled = Some(self.entries.iter().map(|e| e.key).collect());
                let from = current.and_then(|k| self.index(k)).map_or(0, |i| i + 1);
                // Fisher-Yates over the rest.
                for i in (from + 1..self.entries.len()).rev() {
                    let j = from + (self.random() % (i - from + 1) as u64) as usize;
                    self.entries.swap(i, j);
                }
            }
            (false, true) => {
                let order = self.unshuffled.take().unwrap_or_default();
                let mut entries = std::mem::take(&mut self.entries);
                for key in order {
                    if let Some(i) = entries.iter().position(|e| e.key == key) {
                        self.entries.push(entries.swap_remove(i));
                    }
                }
                // Anything the order didn't know about goes last.
                self.entries.extend(entries);
            }
            _ => {}
        }
    }

    /// Move `key` to the front, the rest keeping their order (to start a shuffled
    /// queue with the track that was picked).
    pub fn move_to_front(&mut self, key: Key) {
        if let Some(i) = self.index(key) {
            let e = self.entries.remove(i);
            self.entries.insert(0, e);
        }
    }

    /// What plays after `key`, with `repeat`. `None` means the queue is done.
    pub fn after(&self, key: Key, repeat: Repeat) -> Option<Key> {
        if repeat == Repeat::One {
            return self.get(key).map(|e| e.key);
        }
        self.step(key, 1, if repeat == Repeat::All { Wrap::Yes } else { Wrap::No })
    }

    /// The entry `by` places from `key` (forward or back), wrapping or not.
    pub fn step(&self, key: Key, by: isize, wrap: Wrap) -> Option<Key> {
        let i = self.index(key)? as isize + by;
        let n = self.entries.len() as isize;
        let i = match wrap {
            Wrap::Yes => i.rem_euclid(n),
            Wrap::No if (0..n).contains(&i) => i,
            Wrap::No => return None,
        };
        self.key_at(i as usize)
    }

    /// xorshift64*: good enough to shuffle songs.
    fn random(&mut self) -> u64 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(names: &[&str]) -> Vec<(QueueItem, String)> {
        names
            .iter()
            .map(|n| {
                let item = QueueItem {
                    id: (*n).into(),
                    title: n.to_uppercase(),
                    artist: None,
                    album: None,
                    duration: None,
                    artwork: None,
                };
                (item, format!("hash-{n}"))
            })
            .collect()
    }

    fn ids(q: &Queue) -> Vec<&str> {
        q.entries().iter().map(|e| e.item.id.as_str()).collect()
    }

    #[test]
    fn steps_forward_and_back_with_and_without_repeat() {
        let mut q = Queue::new(1);
        let k = q.replace(items(&["a", "b", "c"]));
        assert_eq!(q.after(k[0], Repeat::Off), Some(k[1]));
        assert_eq!(q.after(k[2], Repeat::Off), None);
        assert_eq!(q.after(k[2], Repeat::All), Some(k[0]));
        assert_eq!(q.after(k[1], Repeat::One), Some(k[1]));
        assert_eq!(q.step(k[0], -1, Wrap::No), None);
        assert_eq!(q.step(k[0], -1, Wrap::Yes), Some(k[2]));
        assert_eq!(q.after(999, Repeat::All), None, "a key that's gone has nothing after it");
    }

    #[test]
    fn enqueues_at_the_end_or_next() {
        let mut q = Queue::new(1);
        let k = q.replace(items(&["a", "b", "c"]));
        q.enqueue(items(&["d"]), None);
        q.enqueue(items(&["n"]), Some(k[0]));
        assert_eq!(ids(&q), ["a", "n", "b", "c", "d"]);
        q.remove(k[1]);
        assert_eq!(ids(&q), ["a", "n", "c", "d"]);
        // The same track twice is two entries.
        q.enqueue(items(&["a"]), None);
        assert_eq!(ids(&q), ["a", "n", "c", "d", "a"]);
        assert_ne!(q.entries()[0].key, q.entries()[4].key);
    }

    #[test]
    fn shuffles_only_whats_after_the_current_track_and_puts_it_back() {
        let names: Vec<String> = (0..50).map(|i| format!("t{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut q = Queue::new(42);
        let k = q.replace(items(&names));
        q.set_shuffle(true, Some(k[9]));
        assert!(q.shuffled());
        assert_eq!(ids(&q)[..10], names[..10], "played and current tracks stay put");
        assert_ne!(ids(&q)[10..], names[10..], "the rest is shuffled");
        let mut rest = ids(&q)[10..].to_vec();
        rest.sort_by_key(|n| n[1..].parse::<u32>().unwrap());
        assert_eq!(rest, names[10..], "nothing lost or doubled");
        // Added while shuffled, removed while shuffled: both kept track of.
        let added = q.enqueue(items(&["new"]), Some(k[9]));
        assert_eq!(q.after(k[9], Repeat::Off), Some(added[0]));
        q.remove(k[20]);
        q.set_shuffle(false, Some(k[9]));
        let mut want: Vec<&str> = names.clone();
        want.insert(10, "new");
        want.retain(|n| *n != "t20");
        assert_eq!(ids(&q), want);
    }

    #[test]
    fn shuffling_with_nothing_playing_shuffles_everything() {
        let names: Vec<String> = (0..20).map(|i| format!("t{i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut q = Queue::new(7);
        q.replace(items(&names));
        q.set_shuffle(true, None);
        assert_ne!(ids(&q), names);
        let last = q.key_at(19).unwrap();
        q.move_to_front(last);
        assert_eq!(q.key_at(0), Some(last));
        assert_eq!(q.len(), 20);
        // A new queue isn't shuffled.
        q.replace(items(&["x"]));
        assert!(!q.shuffled());
    }
}
