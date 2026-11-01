use super::*;

#[test]
fn playing_next_keeps_the_current_track_and_the_import_order() {
    let mut queue: VecDeque<u32> = VecDeque::from([0, 1, 2, 10, 11, 12]);
    play_next(&mut queue, 3);
    assert_eq!(queue, VecDeque::from([0, 10, 11, 12, 1, 2]));

    // A queue that only holds the import has nothing to jump ahead of.
    let mut fresh: VecDeque<u32> = VecDeque::from([10, 11]);
    play_next(&mut fresh, 2);
    assert_eq!(fresh, VecDeque::from([10, 11]));

    // The import already sits directly behind the current track.
    let mut behind: VecDeque<u32> = VecDeque::from([0, 10]);
    play_next(&mut behind, 1);
    assert_eq!(behind, VecDeque::from([0, 10]));

    play_next(&mut VecDeque::<u32>::new(), 0);
}
