//! Deterministic synthetic emails, so runs on the Mac and the Pi are comparable.

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    /// xorshift64*: tiny, fast and good enough for test data.
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

#[rustfmt::skip]
const WORDS: &[&str] = &[
    "meeting", "report", "quarterly", "budget", "lunch", "garden", "invoice", "travel", "photos", "family",
    "project", "deadline", "review", "draft", "concert", "ticket", "weekend", "update", "schedule", "notes",
    "contract", "design", "release", "server", "backup", "recipe", "birthday", "school", "library", "music",
    "the", "a", "and", "to", "of", "for", "with", "on", "about", "please", "thanks", "tomorrow", "today",
    "next", "week", "team", "call", "plan", "idea", "question", "answer", "change", "version", "history",
];

const PEOPLE: &[&str] =
    &["ana", "bo", "chen", "dara", "eli", "fay", "gus", "hana", "ivo", "jo", "kai", "lena"];
const LABELS: &[&str] = &["INBOX", "IMPORTANT", "STARRED", "Family", "Work", "Receipts", "Travel"];

pub struct Email {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub date: i64,
    pub read: bool,
    pub labels: Vec<&'static str>,
}

fn words(rng: &mut Rng, target_bytes: usize) -> String {
    let mut s = String::with_capacity(target_bytes + 16);
    while s.len() < target_bytes {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(WORDS[rng.below(WORDS.len())]);
    }
    s
}

pub fn email(rng: &mut Rng, index: usize, body_bytes: usize) -> Email {
    let mut labels = vec!["INBOX"];
    for label in &LABELS[1..] {
        if rng.chance(15) {
            labels.push(label);
        }
    }
    let subject = words(rng, 40);
    let mut body = words(rng, body_bytes);
    if rng.chance(2) {
        body.push(' ');
        body.push_str(RARE_WORD);
    }
    Email {
        from: format!("{}@example.com", PEOPLE[rng.below(PEOPLE.len())]),
        to: format!("{}@example.com", PEOPLE[rng.below(PEOPLE.len())]),
        subject,
        body,
        // One email every ten minutes, starting 2025-01-01.
        date: 1_735_689_600_000 + index as i64 * 600_000,
        read: rng.chance(70),
        labels,
    }
}

/// A word that appears in roughly 1 in 50 bodies, for search queries.
pub const RARE_WORD: &str = "aardvark";
