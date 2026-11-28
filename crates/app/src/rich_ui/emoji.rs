//! The emoji table behind the picker: categorized groups with light
//! search keywords, plus the recent-emoji book (capped, persisted
//! next to the other app state files). Emoji are message content,
//! never iconography, so this data lives beside the picker that
//! renders it.

/// One category of the picker: a label key for i18n and its emoji
/// with lowercase search keywords (space separated).
pub(crate) struct EmojiGroup {
    pub(crate) label_key: &'static str,
    pub(crate) emoji: &'static [(&'static str, &'static str)],
}

pub(crate) const RECENT_KEY: &str = "emoji-recents.json";
/// The recent strip holds this many emoji, WhatsApp-style.
pub(crate) const RECENT_CAP: usize = 24;

pub(crate) const GROUPS: &[EmojiGroup] = &[
    EmojiGroup {
        label_key: "emoji.smileys_people",
        emoji: &[
            ("😀", "grin happy senyum"),
            ("😃", "smile happy"),
            ("😄", "laugh happy smile"),
            ("😁", "beam grin"),
            ("😆", "lol laugh haha"),
            ("😅", "sweat laugh"),
            ("🤣", "rofl laugh"),
            ("😂", "joy tears laugh"),
            ("🙂", "slight smile"),
            ("😉", "wink"),
            ("😊", "blush smile"),
            ("😇", "angel innocent halo"),
            ("🥰", "love hearts"),
            ("😍", "heart eyes love"),
            ("😘", "kiss"),
            ("😗", "kissing"),
            ("😎", "cool sunglasses"),
            ("🤩", "starstruck wow"),
            ("🥳", "party celebrate"),
            ("😏", "smirk"),
            ("😒", "unamused meh"),
            ("😞", "sad down"),
            ("😔", "sad sigh"),
            ("😟", "worried"),
            ("🙁", "frown sad"),
            ("😣", "struggle"),
            ("😖", "confounded"),
            ("😫", "tired weary"),
            ("😩", "weary"),
            ("🥺", "pleading puppy"),
            ("😢", "cry crying"),
            ("😭", "sob cry loudly"),
            ("😤", "triumph huff angry"),
            ("😠", "angry mad"),
            ("😡", "rage angry"),
            ("🤬", "swear cursing"),
            ("🤯", "mindblown explode"),
            ("😳", "flushed shy"),
            ("🥵", "hot heat"),
            ("🥶", "cold freeze"),
            ("😱", "scream fear"),
            ("😨", "fearful"),
            ("🤗", "hug"),
            ("🤔", "thinking hmm"),
            ("🤫", "shh quiet"),
            ("🤥", "lying pinocchio"),
            ("😐", "neutral meh"),
            ("😑", "expressionless"),
            ("😬", "grimace awkward"),
            ("🙄", "eyeroll"),
            ("😴", "sleep zzz"),
            ("🤤", "drool"),
            ("😪", "sleepy"),
            ("😵", "dizzy knock out"),
            ("🤢", "nauseated sick"),
            ("🤮", "vomit puke"),
            ("🤧", "sneeze"),
            ("😷", "mask sick"),
            ("🤒", "thermometer sick"),
            ("🤕", "bandage hurt"),
            ("🤑", "money face"),
            ("🤠", "cowboy"),
            ("🥸", "disguise"),
            ("👍", "thumbs up like ok bagus"),
            ("👎", "thumbs down dislike"),
            ("👌", "ok perfect"),
            ("✌️", "peace victory"),
            ("🤞", "fingers crossed luck"),
            ("🤟", "love you"),
            ("🤘", "rock horns"),
            ("👏", "clap applause"),
            ("🙌", "raise hands hooray"),
            ("🙏", "pray thanks tolong terima kasih"),
            ("🤝", "handshake deal"),
            ("💪", "muscle strong kuat"),
            ("👀", "eyes look"),
            ("🧠", "brain smart"),
            ("👋", "wave hello hai"),
            ("🤙", "call me"),
            ("👶", "baby"),
            ("🧑", "person"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.animals",
        emoji: &[
            ("🐶", "dog puppy anjing"),
            ("🐱", "cat kucing"),
            ("🐭", "mouse tikus"),
            ("🐹", "hamster"),
            ("🐰", "rabbit bunny kelinci"),
            ("🦊", "fox rubah"),
            ("🐻", "bear beruang"),
            ("🐼", "panda"),
            ("🐨", "koala"),
            ("🐯", "tiger harimau"),
            ("🦁", "lion singa"),
            ("🐮", "cow sapi"),
            ("🐷", "pig babi"),
            ("🐸", "frog katak"),
            ("🐵", "monkey monyet"),
            ("🐔", "chicken ayam"),
            ("🐧", "penguin"),
            ("🐦", "bird burung"),
            ("🦆", "duck bebek"),
            ("🦅", "eagle elang"),
            ("🦉", "owl burung hantu"),
            ("🐴", "horse kuda"),
            ("🦄", "unicorn"),
            ("🐝", "bee lebah"),
            ("🐛", "caterpillar ulat"),
            ("🦋", "butterfly kupu"),
            ("🐌", "snail keong"),
            ("🐞", "ladybug"),
            ("🐜", "ant semut"),
            ("🐢", "turtle kura"),
            ("🐍", "snake ular"),
            ("🐙", "octopus"),
            ("🦀", "crab kepiting"),
            ("🐬", "dolphin lumba"),
            ("🐟", "fish ikan"),
            ("🐳", "whale paus"),
            ("🦈", "shark hiu"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.food",
        emoji: &[
            ("🍎", "apple apel"),
            ("🍊", "orange jeruk"),
            ("🍋", "lemon"),
            ("🍌", "banana pisang"),
            ("🍉", "watermelon semangka"),
            ("🍇", "grapes anggur"),
            ("🍓", "strawberry"),
            ("🫐", "blueberry"),
            ("🍒", "cherry"),
            ("🍑", "peach persik"),
            ("🥭", "mango mangga"),
            ("🍍", "pineapple nanas"),
            ("🥥", "coconut kelapa"),
            ("🥝", "kiwi"),
            ("🍅", "tomato tomat"),
            ("🥑", "avocado alpukat"),
            ("🌽", "corn jagung"),
            ("🥕", "carrot wortel"),
            ("🍞", "bread roti"),
            ("🥐", "croissant"),
            ("🍳", "egg cooking telur"),
            ("🍚", "rice nasi"),
            ("🍜", "noodles ramen mie"),
            ("🍝", "spaghetti pasta"),
            ("🍣", "sushi"),
            ("🍱", "bento"),
            ("🍛", "curry rice kari"),
            ("🍲", "soup stew sup"),
            ("🥗", "salad"),
            ("🍕", "pizza"),
            ("🍔", "burger"),
            ("🌮", "taco"),
            ("🥙", "kebab wrap"),
            ("🍟", "fries kentang"),
            ("🍗", "chicken ayam"),
            ("🍖", "meat daging"),
            ("🍿", "popcorn"),
            ("🧁", "cupcake"),
            ("🍰", "cake kue"),
            ("🎂", "birthday cake ulang tahun"),
            ("🍪", "cookie"),
            ("🍫", "chocolate coklat"),
            ("🍬", "candy permen"),
            ("🍭", "lollipop"),
            ("🍩", "donut"),
            ("☕", "coffee kopi"),
            ("🍵", "tea teh"),
            ("🧋", "boba bubble tea"),
            ("🥤", "soda drink"),
            ("🍺", "beer bir"),
            ("🍷", "wine"),
            ("🥛", "milk susu"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.activity",
        emoji: &[
            ("⚽", "soccer football bola"),
            ("🏀", "basketball basket"),
            ("🏈", "football amerika"),
            ("⚾", "baseball"),
            ("🎾", "tennis"),
            ("🏐", "volleyball voli"),
            ("🏓", "ping pong tennis meja"),
            ("🏸", "badminton"),
            ("🥊", "boxing tinju"),
            ("🎯", "dart target"),
            ("🎲", "dice dadu"),
            ("🎮", "game controller gaming"),
            ("🕹️", "joystick arcade"),
            ("🎧", "headphones musik"),
            ("🎤", "microphone karaoke mic"),
            ("🎸", "guitar"),
            ("🎹", "piano keyboard"),
            ("🥁", "drum"),
            ("🎺", "trumpet"),
            ("🎻", "violin"),
            ("🎬", "movie film"),
            ("🎨", "art paint lukis"),
            ("🏆", "trophy win juara"),
            ("🥇", "gold medal emas"),
            ("🎉", "party confetti celebrate"),
            ("🎊", "sparkler celebration"),
            ("🎈", "balloon balon"),
            ("🎁", "gift hadiah"),
            ("🏃", "run lari"),
            ("🚴", "bike cycling sepeda"),
            ("🏋️", "gym weights"),
            ("🧘", "yoga meditate"),
            ("🏊", "swim renang"),
            ("🏄", "surf"),
            ("⛰️", "mountain gunung"),
            ("🏕️", "camping tent kemah"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.travel",
        emoji: &[
            ("🚗", "car mobil"),
            ("🚕", "taxi"),
            ("🚌", "bus"),
            ("🚎", "trolley bus"),
            ("🏎️", "race car"),
            ("🚓", "police car"),
            ("🚑", "ambulance"),
            ("🚒", "fire truck"),
            ("🛻", "pickup"),
            ("🚚", "truck truk"),
            ("🚛", "articulated lorry"),
            ("🛵", "scooter"),
            ("🏍️", "motorcycle motor"),
            ("🚲", "bicycle sepeda"),
            ("🛴", "kick scooter"),
            ("✈️", "airplane pesawat"),
            ("🚁", "helicopter heli"),
            ("🚀", "rocket roket"),
            ("🛰️", "satellite"),
            ("🚂", "train locomotive kereta"),
            ("🚆", "train railway"),
            ("🚇", "metro subway"),
            ("🚢", "ship kapal"),
            ("⛵", "sailboat perahu"),
            ("🛶", "canoe"),
            ("🏝️", "island pulau"),
            ("🏖️", "beach pantai"),
            ("🏔️", "mountain snow gunung"),
            ("🌋", "volcano gunung api"),
            ("🗾", "japan map"),
            ("🏠", "house home rumah"),
            ("🏢", "office building"),
            ("🏥", "hospital rumah sakit"),
            ("🏫", "school sekolah"),
            ("🕌", "mosque masjid"),
            ("⛩️", "shrine"),
            ("🗺️", "map peta"),
            ("🧭", "compass kompas"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.objects",
        emoji: &[
            ("⌚", "watch jam"),
            ("📱", "phone hp ponsel"),
            ("💻", "laptop computer"),
            ("🖥️", "desktop computer"),
            ("⌨️", "keyboard"),
            ("🖨️", "printer"),
            ("🖱️", "mouse"),
            ("💾", "floppy save"),
            ("💿", "cd disc"),
            ("📷", "camera kamera"),
            ("📹", "video camera"),
            ("📺", "tv television"),
            ("📻", "radio"),
            ("⏰", "alarm clock jam weker"),
            ("⏳", "hourglass sand"),
            ("🔋", "battery baterai"),
            ("💡", "light bulb idea lampu"),
            ("🔌", "plug stopkontak"),
            ("📚", "books buku"),
            ("📖", "open book"),
            ("📝", "note memo tulis"),
            ("✏️", "pencil pensil"),
            ("🖊️", "pen pulpen"),
            ("📁", "folder"),
            ("📅", "calendar kalender"),
            ("📌", "pin pushpin"),
            ("📎", "clip paperclip"),
            ("🔒", "lock kunci"),
            ("🔑", "key kunci"),
            ("🔨", "hammer palu"),
            ("🛠️", "tools"),
            ("🔧", "wrench kunci inggris"),
            ("💊", "pill obat"),
            ("💰", "money bag uang"),
            ("💳", "credit card kartu"),
            ("💎", "gem diamond berlian"),
            ("🛒", "cart shopping belanja"),
            ("🎒", "backpack tas"),
            ("👟", "shoe sneaker sepatu"),
            ("👕", "shirt tshirt baju"),
            ("👗", "dress"),
            ("🧢", "cap topi"),
            ("👓", "glasses kacamata"),
            ("🌂", "umbrella payung"),
            ("🛁", "bath mandi"),
            ("🧴", "lotion"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.symbols",
        emoji: &[
            ("❤️", "red heart love cinta"),
            ("🧡", "orange heart"),
            ("💛", "yellow heart"),
            ("💚", "green heart"),
            ("💙", "blue heart"),
            ("💜", "purple heart"),
            ("🖤", "black heart"),
            ("🤍", "white heart"),
            ("💔", "broken heart patah"),
            ("❣️", "heart exclamation"),
            ("💯", "hundred perfect score"),
            ("💥", "boom collision"),
            ("💫", "dizzy star"),
            ("✨", "sparkles shine"),
            ("⭐", "star bintang"),
            ("🌟", "glowing star"),
            ("🔥", "fire hot api"),
            ("🌈", "rainbow pelangi"),
            ("☀️", "sun matahari"),
            ("🌤️", "cloud sun"),
            ("☁️", "cloud awan"),
            ("🌧️", "rain hujan"),
            ("⛈️", "storm petir"),
            ("❄️", "snow salju"),
            ("🌊", "wave ocean ombak"),
            ("✅", "check yes benar"),
            ("❌", "cross no salah"),
            ("❓", "question tanya"),
            ("❗", "exclamation seru"),
            ("⚠️", "warning awas"),
            ("🚫", "forbidden no"),
            ("♻️", "recycle"),
            ("🔔", "bell notif lonceng"),
            ("🔕", "bell off"),
            ("🎵", "music note musik"),
            ("🎶", "music notes"),
            ("💤", "zzz sleep"),
            ("💬", "speech chat"),
            ("👁️‍🗨️", "eye bubble"),
            ("🕐", "one oclock clock"),
        ],
    },
    EmojiGroup {
        label_key: "emoji.flags",
        emoji: &[
            ("🇮🇩", "indonesia flag bendera"),
            ("🇲🇾", "malaysia flag"),
            ("🇸🇬", "singapore flag"),
            ("🇯🇵", "japan flag jepang"),
            ("🇰🇷", "korea flag"),
            ("🇨🇳", "china flag"),
            ("🇺🇸", "usa flag amerika"),
            ("🇬🇧", "uk flag inggris"),
            ("🇩🇪", "germany flag jerman"),
            ("🇫🇷", "france flag prancis"),
            ("🇳🇱", "netherlands flag belanda"),
            ("🇸🇦", "saudi flag"),
            ("🇹🇷", "turkey flag"),
            ("🇦🇺", "australia flag"),
            ("🇧🇷", "brazil flag"),
            ("🇮🇳", "india flag"),
            ("🏁", "checkered flag finish"),
            ("🚩", "triangular flag merah"),
            ("🎌", "crossed flags"),
        ],
    },
];

/// True when `emoji`'s keywords (or the character itself) match the
/// lowercased `query`.
pub(crate) fn matches(emoji: &(&'static str, &'static str), query: &str) -> bool {
    query.is_empty()
        || emoji.0.contains(query)
        || emoji.1.split_whitespace().any(|k| k.contains(query))
}

/// Loads the recent-emoji list from disk (empty on first run).
pub(crate) fn load_recents() -> Vec<String> {
    let path = crate::extras::feature_state_dir().join(RECENT_KEY);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<String>>(&text).ok())
        .unwrap_or_default()
}

/// Persists the recent-emoji list, tolerating a failed write the
/// same way the other state files do.
pub(crate) fn save_recents(recents: &[String]) {
    let dir = crate::extras::feature_state_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string(recents) {
        let _ = std::fs::write(dir.join(RECENT_KEY), text);
    }
}

/// Pushes `emoji` to the front of the recents, deduped and capped,
/// then persists. The pure part lives in [`push_recent`] so tests
/// never touch the real state file.
pub(crate) fn note_used(recents: &mut Vec<String>, emoji: &str) {
    push_recent(recents, emoji);
    save_recents(recents);
}

/// The in-memory part of [`note_used`]: dedupe, front-insert, cap.
fn push_recent(recents: &mut Vec<String>, emoji: &str) {
    recents.retain(|e| e != emoji);
    recents.insert(0, emoji.to_owned());
    recents.truncate(RECENT_CAP);
}

#[cfg(test)]
mod tests {
    use super::{matches, push_recent, GROUPS, RECENT_CAP};

    #[test]
    fn groups_are_labeled_and_nonempty() {
        assert!(GROUPS.len() >= 8);
        for group in GROUPS {
            assert!(!group.emoji.is_empty(), "{} has no emoji", group.label_key);
            for (ch, keywords) in group.emoji {
                // The cap admits ZWJ sequences (the eye-in-speech
                // bubble is five code points joined by two ZWJs).
                assert!(ch.chars().count() <= 8, "not an emoji: {ch}");
                assert!(!keywords.is_empty(), "{ch} has no keywords");
            }
        }
    }

    #[test]
    fn search_matches_keywords_and_characters() {
        let thumbs = GROUPS[0].emoji.iter().find(|e| e.0 == "👍").expect("thumbs up");
        assert!(matches(thumbs, "thumbs"));
        assert!(matches(thumbs, "bagus"));
        assert!(matches(thumbs, "👍"));
        assert!(!matches(thumbs, "cat"));
        let cat = GROUPS[1].emoji.iter().find(|e| e.0 == "🐱").expect("cat");
        assert!(matches(cat, "kuc"));
    }

    #[test]
    fn recents_dedupe_cap_and_order() {
        // Uses the pure in-memory half: persisting test data into
        // the real state file would leak into the user's picker.
        let mut recents = Vec::new();
        push_recent(&mut recents, "😀");
        push_recent(&mut recents, "👍");
        push_recent(&mut recents, "😀");
        assert_eq!(recents.first().map(String::as_str), Some("😀"));
        assert_eq!(recents.len(), 2);
        for i in 0..RECENT_CAP + 5 {
            push_recent(&mut recents, &format!("e{i}"));
        }
        assert_eq!(recents.len(), RECENT_CAP);
    }
}
