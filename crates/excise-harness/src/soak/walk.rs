//! The walk to a folder: where the arrow keys have led, and which key to press next.
//!
//! At the end of a scan the program puts the cursor on the largest entry. When that is a file, the
//! soak moves the cursor to a folder with the arrow keys before it opens one. The map is a
//! treemap, not a list, so there is no order to follow, and the only thing a key tells the soak is
//! the entry that the inspector shows afterwards. An entry is known by what the inspector shows of
//! it as a whole (its pane: name, sizes, item check) and not by its name, which the program cuts
//! to fit and which two entries can share; the walk takes that text for the entry and does not
//! look into it. [`Walk`] is the soak's memory of that: for each entry the cursor has been on,
//! what each of the four arrow keys did from there (nothing, or a move to another entry). It asks
//! for keys in an order that explores the whole map that the cursor can reach:
//!
//! * a key not yet tried from the entry the cursor is on, in the order right, down, left, up: the
//!   map is laid out largest first from the top left, so the entries after the largest are usually
//!   to its right or below it;
//! * when every key has been tried from there, the keys that lead, by what is known, to the
//!   nearest entry that still has a key not yet tried, which is how the walk gets back to an entry
//!   it left (a key that went one way does not always come back the other: the cursor moves to
//!   the nearest tile in a direction, and that is not always the tile it came from).
//!
//! A key is tried from an entry once. The walk is over when none of the entries it can still get
//! to has a key left to try ([`Next::Done`]): the map has been walked all around. If an entry with
//! a key left is out of reach, because no known key leads back to it, the walk does not know
//! that the map was walked all around, and says so ([`Next::Stuck`]) instead of claiming it.
//!
//! This is all bookkeeping and presses nothing: the driver presses the keys and reads the
//! inspector, so every key still goes through the choke point.

use std::collections::{BTreeMap, VecDeque};

use super::keys::Key;

/// An arrow key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Arrow {
    Right,
    Down,
    Left,
    Up,
}

impl Arrow {
    /// The arrow keys in the order the walk tries them from an entry it has not been on before.
    pub(super) const ALL: [Self; 4] = [Self::Right, Self::Down, Self::Left, Self::Up];

    /// The key the soak presses for the arrow: one of its allowlist.
    pub(super) const fn key(self) -> Key {
        match self {
            Self::Right => Key::RIGHT,
            Self::Down => Key::DOWN,
            Self::Left => Key::LEFT,
            Self::Up => Key::UP,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// What an arrow key did from an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Led {
    /// The cursor stayed on the entry.
    Nowhere,
    /// The cursor moved to the entry with this name.
    To(String),
}

/// What the walk asks for next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// Press this key.
    Press(Arrow),
    /// Every key has been tried from every entry the cursor can get to, and none led to a folder:
    /// the map has been walked all around.
    Done,
    /// Some entry has a key that was never tried from it, and no key that is known leads back to
    /// it: the walk cannot say that the map was walked all around.
    Stuck,
}

/// Where the arrow keys have led, from the entries the cursor has been on.
#[derive(Debug)]
pub(super) struct Walk {
    /// For every entry the cursor has been on, what each key did from it, in the order of
    /// [`Arrow::ALL`]; `None` for a key not yet tried from there.
    seen: BTreeMap<String, [Option<Led>; 4]>,
    /// The entry the cursor is on.
    here: String,
}

impl Walk {
    /// A walk that starts with the cursor on the entry `start`.
    pub(super) fn new(start: &str) -> Self {
        Self {
            seen: BTreeMap::from([(start.to_owned(), Default::default())]),
            here: start.to_owned(),
        }
    }

    /// How many entries the cursor has been on.
    pub(super) fn entries(&self) -> usize {
        self.seen.len()
    }

    /// The key to press next, or why there is none.
    pub(super) fn next(&self) -> Next {
        if let Some(arrow) = self.untried_from(&self.here) {
            return Next::Press(arrow);
        }
        // Breadth first, along the keys that are known to lead somewhere. For each entry reached,
        // `reached` holds the key to press now, from where the cursor is, to start on the way
        // there.
        let mut reached: BTreeMap<&str, Arrow> = BTreeMap::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        self.follow(&self.here, None, &mut reached, &mut queue);
        while let Some(entry) = queue.pop_front() {
            let Some(&first) = reached.get(entry) else {
                continue;
            };
            if self.untried_from(entry).is_some() {
                return Next::Press(first);
            }
            self.follow(entry, Some(first), &mut reached, &mut queue);
        }
        let out_of_reach = self
            .seen
            .values()
            .any(|edges| edges.iter().any(Option::is_none));
        if out_of_reach {
            Next::Stuck
        } else {
            Next::Done
        }
    }

    /// Records that `arrow` was pressed with the cursor on the entry the walk is at, and that the
    /// inspector now shows `now_on`.
    pub(super) fn observe(&mut self, arrow: Arrow, now_on: &str) {
        let led = if now_on == self.here {
            Led::Nowhere
        } else {
            Led::To(now_on.to_owned())
        };
        if let Some(edges) = self.seen.get_mut(&self.here) {
            edges[arrow.index()] = Some(led);
        }
        self.seen.entry(now_on.to_owned()).or_default();
        now_on.clone_into(&mut self.here);
    }

    /// The first arrow key not yet tried from `entry`, in the order of [`Arrow::ALL`].
    fn untried_from(&self, entry: &str) -> Option<Arrow> {
        let edges = self.seen.get(entry)?;
        Arrow::ALL
            .into_iter()
            .find(|arrow| edges[arrow.index()].is_none())
    }

    /// Adds to `reached` and `queue` every entry that a key tried from `entry` led to and that is
    /// not there yet. The key that starts the way there is `first`, or, from the entry the cursor
    /// is on (`first` is `None`), the key that led there.
    fn follow<'a>(
        &'a self,
        entry: &str,
        first: Option<Arrow>,
        reached: &mut BTreeMap<&'a str, Arrow>,
        queue: &mut VecDeque<&'a str>,
    ) {
        let Some(edges) = self.seen.get(entry) else {
            return;
        };
        for arrow in Arrow::ALL {
            let Some(Led::To(target)) = &edges[arrow.index()] else {
                continue;
            };
            if *target == self.here || reached.contains_key(target.as_str()) {
                continue;
            }
            reached.insert(target.as_str(), first.unwrap_or(arrow));
            queue.push_back(target.as_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A map as the cursor sees it: which entry each arrow key leads to from each entry, and
    /// which entries are folders. A key with no row leaves the cursor where it is.
    struct Map {
        moves: BTreeMap<(&'static str, usize), &'static str>,
        folders: Vec<&'static str>,
    }

    impl Map {
        fn new(moves: &[(&'static str, Arrow, &'static str)], folders: &[&'static str]) -> Self {
            Self {
                moves: moves
                    .iter()
                    .map(|(from, arrow, to)| ((*from, arrow.index()), *to))
                    .collect(),
                folders: folders.to_vec(),
            }
        }

        fn press(&self, from: &'static str, arrow: Arrow) -> &'static str {
            self.moves
                .get(&(from, arrow.index()))
                .copied()
                .unwrap_or(from)
        }
    }

    /// How a walk over a map ended.
    #[derive(Debug, PartialEq, Eq)]
    enum Ended {
        Folder(&'static str),
        Done,
        Stuck,
    }

    /// Walks `map` from `start` the way the driver does, and says how it ended, which keys it
    /// pressed, and what it remembered.
    fn explore(map: &Map, start: &'static str, budget: usize) -> (Ended, Vec<Arrow>, Walk) {
        let mut walk = Walk::new(start);
        let mut here = start;
        let mut pressed = Vec::new();
        loop {
            let arrow = match walk.next() {
                Next::Press(arrow) => arrow,
                Next::Done => return (Ended::Done, pressed, walk),
                Next::Stuck => return (Ended::Stuck, pressed, walk),
            };
            assert!(
                pressed.len() < budget,
                "the walk pressed more than {budget} keys"
            );
            pressed.push(arrow);
            here = map.press(here, arrow);
            if map.folders.contains(&here) {
                return (Ended::Folder(here), pressed, walk);
            }
            walk.observe(arrow, here);
        }
    }

    #[test]
    fn the_keys_are_tried_from_a_new_entry_in_the_order_right_down_left_up() {
        let mut walk = Walk::new("big");
        for arrow in Arrow::ALL {
            assert_eq!(walk.next(), Next::Press(arrow));
            walk.observe(arrow, "big");
        }
        assert_eq!(walk.next(), Next::Done);
        assert_eq!(walk.entries(), 1);
    }

    #[test]
    fn the_arrows_are_keys_of_the_allowlist() {
        for arrow in Arrow::ALL {
            assert!(Key::ALLOWED.contains(&arrow.key()), "{}", arrow.key());
        }
    }

    #[test]
    fn a_folder_below_the_largest_entry_is_reached_after_a_detour_to_the_right() {
        // The largest file `a` is at the upper left, a file `b` as high as the map is to its
        // right, and a folder `f` is below `a`. Right reaches `b`, which has nothing to its right
        // or below it; left goes back to `a`, and the key not yet tried from `a` is down. A walk
        // that counted four keys that led nowhere new, whichever the entry, would try `a`'s up
        // key next and stop, and never try its down key.
        let map = Map::new(
            &[
                ("a", Arrow::Right, "b"),
                ("a", Arrow::Down, "f"),
                ("b", Arrow::Left, "a"),
            ],
            &["f"],
        );

        let (ended, pressed, _) = explore(&map, "a", 48);

        assert_eq!(ended, Ended::Folder("f"));
        assert_eq!(
            pressed,
            [
                Arrow::Right, // a to b
                Arrow::Right, // b stays
                Arrow::Down,  // b stays
                Arrow::Left,  // b to a
                Arrow::Down,  // a to f
            ]
        );
    }

    #[test]
    fn a_map_of_files_alone_is_walked_all_around_with_every_key_tried_from_every_entry() {
        // Two files side by side: right from `a` reaches `b`, and left from `b` reaches `a`.
        let map = Map::new(&[("a", Arrow::Right, "b"), ("b", Arrow::Left, "a")], &[]);

        let (ended, pressed, walk) = explore(&map, "a", 48);

        assert_eq!(ended, Ended::Done);
        assert_eq!(walk.entries(), 2);
        // Four keys from each of the two entries, and one more to go from `a` back to `b`, which
        // still had its up key.
        assert_eq!(pressed.len(), 9, "{pressed:?}");
    }

    #[test]
    fn a_walk_goes_back_along_the_keys_it_knows_to_an_entry_that_has_a_key_left() {
        // A ring: right leads from `a` to `b`, from `b` to `c`, and from `c` back to `a`; every
        // other key leads nowhere. No key leads back the way it came, so the way back to an
        // entry with a key left is always forward.
        let map = Map::new(
            &[
                ("a", Arrow::Right, "b"),
                ("b", Arrow::Right, "c"),
                ("c", Arrow::Right, "a"),
            ],
            &[],
        );

        let (ended, pressed, walk) = explore(&map, "a", 48);

        assert_eq!(ended, Ended::Done);
        assert_eq!(walk.entries(), 3);
        // Twelve keys to try (four from each entry), and two to go from `a` to `b` and from `b`
        // to `c`, each to an entry that still had keys left.
        assert_eq!(pressed.len(), 14, "{pressed:?}");
    }

    #[test]
    fn an_entry_that_no_known_key_leads_back_to_is_not_taken_for_walked_around() {
        // From `a`, right reaches `b`; from `b`, left reaches `c`, and from `c` every key leads
        // nowhere. `a` has three keys never tried and the cursor cannot get back to it.
        let map = Map::new(&[("a", Arrow::Right, "b"), ("b", Arrow::Left, "c")], &[]);

        let (ended, _, walk) = explore(&map, "a", 48);

        assert_eq!(ended, Ended::Stuck);
        assert_eq!(walk.entries(), 3);
    }

    #[test]
    fn a_key_that_leads_somewhere_else_this_time_is_believed() {
        // The cursor may remember where it was, so the same key from the same entry does not
        // always lead to the same entry. The walk takes the latest answer, and goes on.
        let mut walk = Walk::new("a");
        walk.observe(Arrow::Right, "b");
        walk.observe(Arrow::Left, "a");
        // `a`'s right key is known to lead to `b`; pressed again, it leads to `c`.
        walk.observe(Arrow::Right, "c");

        assert_eq!(walk.entries(), 3);
        // The cursor is on `c`, which has every key left.
        assert_eq!(walk.next(), Next::Press(Arrow::Right));
    }

    #[test]
    fn a_walk_over_a_row_of_entries_stays_within_a_bound_on_its_keys() {
        // A row of ten files: right leads to the next and left to the one before.
        let names: [&'static str; 10] =
            ["e0", "e1", "e2", "e3", "e4", "e5", "e6", "e7", "e8", "e9"];
        let mut moves = Vec::new();
        for pair in names.windows(2) {
            moves.push((pair[0], Arrow::Right, pair[1]));
            moves.push((pair[1], Arrow::Left, pair[0]));
        }
        let map = Map::new(&moves, &[]);

        let (ended, pressed, walk) = explore(&map, "e0", 100);

        assert_eq!(ended, Ended::Done);
        assert_eq!(walk.entries(), names.len());
        // Four keys to try from each entry, and at most one more to route to each.
        assert!(
            pressed.len() <= 6 * names.len(),
            "{} keys for {} entries",
            pressed.len(),
            names.len()
        );
    }
}
