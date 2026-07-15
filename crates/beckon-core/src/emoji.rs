//! Curated emoji and symbol picker: embedded table plus keyword search.
//!
//! The table is deliberately curated, not exhaustive. Full Unicode
//! coverage would bury the useful entries under thousands nobody types;
//! curation IS the feature. Sections, in table order: smileys (roughly
//! most-used first, so the empty query shows them as the stable head),
//! hand gestures, hearts, animals, food and drink, weather and nature,
//! objects, flags for major locales, and a typographic symbols section
//! (arrows, math, box drawing, quotes, dashes, daggers, currency, and
//! mac keyboard glyphs). Names and keywords are lowercase ASCII.
//!
//! Search is deterministic integer scoring: substring matches on the name
//! and keywords, with bonuses for exact, prefix, and word-prefix hits.
//! Ties break by table order, so the same query always returns the same
//! list in the same order.

/// One entry in the picker table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmojiEntry {
    /// The glyph itself, ready to paste.
    pub glyph: &'static str,
    /// Lowercase ASCII display name; the primary search target.
    pub name: &'static str,
    /// Space-separated lowercase ASCII search keywords.
    pub keywords: &'static str,
}

/// Compact table-entry constructor.
const fn e(glyph: &'static str, name: &'static str, keywords: &'static str) -> EmojiEntry {
    EmojiEntry {
        glyph,
        name,
        keywords,
    }
}

/// Score for a query equal to the whole name.
const NAME_EXACT: i64 = 1000;
/// Score for a name starting with the query.
const NAME_PREFIX: i64 = 400;
/// Score for any name word starting with the query.
const NAME_WORD_PREFIX: i64 = 250;
/// Score for the query appearing anywhere in the name.
const NAME_SUBSTRING: i64 = 100;
/// Score for a keyword equal to the query.
const KW_EXACT: i64 = 180;
/// Score for a keyword starting with the query.
const KW_PREFIX: i64 = 120;
/// Score for the query appearing anywhere in the keywords.
const KW_SUBSTRING: i64 = 40;

/// The embedded picker table. Order matters twice: the head of the table
/// is what an empty query shows, and table order breaks score ties.
pub static TABLE: &[EmojiEntry] = &[
    // --- smileys, most-used first ---
    e(
        "😂",
        "face with tears of joy",
        "laugh lol funny crying tears",
    ),
    e(
        "🤣",
        "rolling on the floor laughing",
        "rofl lol laugh funny",
    ),
    e(
        "😊",
        "smiling face with smiling eyes",
        "happy blush warm smile",
    ),
    e("😍", "smiling face with heart eyes", "love crush adore"),
    e("😭", "loudly crying face", "sob cry sad tears"),
    e("😘", "face blowing a kiss", "kiss love mwah"),
    e("🥰", "smiling face with hearts", "love adore happy"),
    e("😅", "grinning face with sweat", "phew nervous laugh"),
    e("😁", "beaming face with smiling eyes", "grin happy teeth"),
    e("😀", "grinning face", "smile happy grin"),
    e("😃", "grinning face with big eyes", "smile happy"),
    e(
        "😄",
        "grinning face with smiling eyes and open mouth",
        "smile happy laugh",
    ),
    e("😆", "grinning squinting face", "laugh haha xd"),
    e("🙂", "slightly smiling face", "smile ok fine"),
    e("🙃", "upside down face", "silly sarcasm irony"),
    e("😉", "winking face", "wink flirt"),
    e("😌", "relieved face", "calm content phew"),
    e("😋", "face savoring food", "yum tasty delicious"),
    e("😛", "face with tongue", "tongue playful"),
    e("😜", "winking face with tongue", "crazy playful joke"),
    e("🤪", "zany face", "crazy wild goofy"),
    e("😝", "squinting face with tongue", "tongue playful gross"),
    e("🤑", "money mouth face", "rich money dollar"),
    e("🤗", "smiling face with open hands", "hug warm thanks"),
    e("🤭", "face with hand over mouth", "oops giggle shy"),
    e("🤫", "shushing face", "quiet secret hush"),
    e("🤔", "thinking face", "hmm think wonder"),
    e("🤨", "face with raised eyebrow", "skeptical suspicious hmm"),
    e("😐", "neutral face", "meh blank"),
    e("😑", "expressionless face", "blank meh"),
    e("😶", "face without mouth", "silent speechless"),
    e("😏", "smirking face", "smug smirk flirt"),
    e("😒", "unamused face", "meh annoyed"),
    e("🙄", "face with rolling eyes", "eyeroll whatever ugh"),
    e("😬", "grimacing face", "awkward yikes"),
    e("🤥", "lying face", "pinocchio liar"),
    e("😔", "pensive face", "sad thoughtful"),
    e("😪", "sleepy face", "tired sleep"),
    e("🤤", "drooling face", "drool hungry"),
    e("😴", "sleeping face", "sleep zzz tired"),
    e("😷", "face with medical mask", "sick mask ill"),
    e("🤒", "face with thermometer", "sick fever ill"),
    e("🤕", "face with head bandage", "hurt injured"),
    e("🤢", "nauseated face", "sick gross green"),
    e("🤮", "face vomiting", "sick puke gross"),
    e("🤧", "sneezing face", "sick sneeze achoo"),
    e("🥵", "hot face", "heat sweating burning"),
    e("🥶", "cold face", "freezing frozen ice"),
    e("🥴", "woozy face", "dizzy drunk tipsy"),
    e("😵", "face with crossed out eyes", "dizzy dead knocked"),
    e("🤯", "exploding head", "mind blown shocked"),
    e("🤠", "cowboy hat face", "yeehaw western"),
    e("🥳", "partying face", "party celebration birthday"),
    e("😎", "smiling face with sunglasses", "cool shades"),
    e("🤓", "nerd face", "geek glasses smart"),
    e("🧐", "face with monocle", "fancy inspect posh"),
    e("😕", "confused face", "puzzled unsure"),
    e("😟", "worried face", "anxious concern"),
    e("🙁", "slightly frowning face", "sad frown"),
    e("😮", "face with open mouth", "wow surprised gasp"),
    e("😲", "astonished face", "shocked amazed"),
    e("😳", "flushed face", "blush embarrassed"),
    e("🥺", "pleading face", "puppy eyes beg cute"),
    e("😢", "crying face", "sad cry tear"),
    e("😤", "face with steam from nose", "frustrated triumph huff"),
    e("😠", "angry face", "mad grr"),
    e("😡", "enraged face", "furious rage mad"),
    e("🤬", "face with symbols on mouth", "swearing cursing angry"),
    e("😈", "smiling face with horns", "devil evil mischief"),
    e("👿", "angry face with horns", "devil imp evil"),
    e("💀", "skull", "dead death skeleton"),
    e("☠️", "skull and crossbones", "death danger poison"),
    e("💩", "pile of poo", "poop crap funny"),
    e("🤡", "clown face", "circus creepy"),
    e("👻", "ghost", "spooky halloween boo"),
    e("👽", "alien", "ufo extraterrestrial space"),
    e("🤖", "robot", "bot machine ai"),
    e("🎃", "jack o lantern", "pumpkin halloween"),
    e("😺", "grinning cat", "cat smile"),
    e("😹", "cat with tears of joy", "cat laugh lol"),
    e("😻", "smiling cat with heart eyes", "cat love"),
    // --- hand gestures ---
    e("👍", "thumbs up", "like yes approve ok good"),
    e("👎", "thumbs down", "dislike no bad"),
    e("👋", "waving hand", "wave hello goodbye hi bye"),
    e("✋", "raised hand", "stop high five palm"),
    e("🖖", "vulcan salute", "spock star trek"),
    e("👌", "ok hand", "okay perfect nice"),
    e("🤌", "pinched fingers", "italian chef gesture"),
    e("🤏", "pinching hand", "small tiny bit"),
    e("✌️", "victory hand", "peace two fingers"),
    e("🤞", "crossed fingers", "luck hope wish"),
    e("🤟", "love you gesture", "rock love asl"),
    e("🤘", "sign of the horns", "rock metal"),
    e("🤙", "call me hand", "shaka hang loose"),
    e("👈", "backhand index pointing left", "left point"),
    e("👉", "backhand index pointing right", "right point"),
    e("👆", "backhand index pointing up", "up point"),
    e("👇", "backhand index pointing down", "down point"),
    e("☝️", "index pointing up", "one point up"),
    e("🖕", "middle finger", "rude flip off"),
    e("✊", "raised fist", "power protest bump"),
    e("👊", "oncoming fist", "punch bro fist bump"),
    e("🤛", "left facing fist", "fist bump"),
    e("🤜", "right facing fist", "fist bump"),
    e("👏", "clapping hands", "clap applause bravo"),
    e("🙌", "raising hands", "hooray praise celebration"),
    e("👐", "open hands", "hug"),
    e("🤝", "handshake", "deal agreement shake"),
    e("🙏", "folded hands", "please thanks pray namaste"),
    e("✍️", "writing hand", "write pen"),
    e("💅", "nail polish", "nails sassy manicure"),
    e("🤳", "selfie", "phone camera"),
    e("💪", "flexed biceps", "muscle strong gym flex"),
    // --- hearts ---
    e("❤️", "red heart", "love heart"),
    e("🧡", "orange heart", "love"),
    e("💛", "yellow heart", "love friendship"),
    e("💚", "green heart", "love nature"),
    e("💙", "blue heart", "love trust"),
    e("💜", "purple heart", "love"),
    e("🖤", "black heart", "love dark"),
    e("🤍", "white heart", "love pure"),
    e("🤎", "brown heart", "love"),
    e("💔", "broken heart", "heartbreak sad breakup"),
    e("❣️", "heart exclamation", "love punctuation"),
    e("💕", "two hearts", "love couple"),
    e("💞", "revolving hearts", "love couple"),
    e("💓", "beating heart", "love pulse"),
    e("💗", "growing heart", "love excited"),
    e("💖", "sparkling heart", "love sparkle"),
    e("💘", "heart with arrow", "love cupid valentine"),
    e("💝", "heart with ribbon", "love gift valentine"),
    // --- animals ---
    e("🐶", "dog face", "puppy pet"),
    e("🐱", "cat face", "kitten pet"),
    e("🐭", "mouse face", "rodent"),
    e("🐹", "hamster", "pet rodent"),
    e("🐰", "rabbit face", "bunny"),
    e("🦊", "fox", "clever"),
    e("🐻", "bear", "grizzly"),
    e("🐼", "panda", "china bamboo"),
    e("🐨", "koala", "australia"),
    e("🐯", "tiger face", "stripes"),
    e("🦁", "lion", "king mane"),
    e("🐮", "cow face", "moo farm"),
    e("🐷", "pig face", "oink farm"),
    e("🐸", "frog", "toad ribbit"),
    e("🐵", "monkey face", "ape"),
    e("🙈", "see no evil monkey", "monkey hide eyes"),
    e("🙉", "hear no evil monkey", "monkey ears"),
    e("🙊", "speak no evil monkey", "monkey mouth secret"),
    e("🐔", "chicken", "hen farm"),
    e("🐧", "penguin", "cold antarctica"),
    e("🐦", "bird", "tweet"),
    e("🐤", "baby chick", "chick cute"),
    e("🦆", "duck", "quack"),
    e("🦅", "eagle", "bird america"),
    e("🦉", "owl", "wise night bird"),
    e("🦇", "bat", "vampire night"),
    e("🐺", "wolf", "howl"),
    e("🐴", "horse face", "pony"),
    e("🦄", "unicorn", "magic fantasy rainbow"),
    e("🐝", "honeybee", "bee buzz"),
    e("🐛", "bug", "insect caterpillar"),
    e("🦋", "butterfly", "pretty insect"),
    e("🐌", "snail", "slow"),
    e("🐞", "lady beetle", "ladybug insect"),
    e("🐢", "turtle", "slow tortoise"),
    e("🐍", "snake", "slither serpent"),
    e("🦎", "lizard", "gecko reptile"),
    e("🐙", "octopus", "tentacles sea"),
    e("🦀", "crab", "sea shellfish"),
    e("🐟", "fish", "sea"),
    e("🐬", "dolphin", "sea flipper"),
    e("🐳", "spouting whale", "sea whale"),
    e("🦈", "shark", "sea jaws"),
    e("🐊", "crocodile", "alligator"),
    e("🐘", "elephant", "trunk"),
    e("🦒", "giraffe", "tall neck"),
    e("🐑", "ewe", "sheep wool"),
    e("🐐", "goat", "farm greatest"),
    e("🕊️", "dove", "peace bird"),
    // --- food and drink ---
    e("🍎", "red apple", "fruit"),
    e("🍊", "tangerine", "orange fruit"),
    e("🍋", "lemon", "sour fruit"),
    e("🍌", "banana", "fruit"),
    e("🍉", "watermelon", "fruit summer"),
    e("🍇", "grapes", "fruit wine"),
    e("🍓", "strawberry", "fruit berry"),
    e("🍒", "cherries", "fruit cherry"),
    e("🍑", "peach", "fruit"),
    e("🥭", "mango", "fruit tropical"),
    e("🍍", "pineapple", "fruit tropical"),
    e("🥥", "coconut", "tropical"),
    e("🍅", "tomato", "vegetable fruit"),
    e("🥑", "avocado", "guacamole toast"),
    e("🥦", "broccoli", "vegetable"),
    e("🥕", "carrot", "vegetable"),
    e("🌽", "ear of corn", "corn maize vegetable"),
    e("🌶️", "hot pepper", "spicy chili"),
    e("🥔", "potato", "vegetable"),
    e("🍞", "bread", "loaf toast"),
    e("🥐", "croissant", "french pastry"),
    e("🥨", "pretzel", "snack"),
    e("🧀", "cheese wedge", "cheese dairy"),
    e("🥚", "egg", "breakfast"),
    e("🍳", "cooking", "fried egg breakfast pan"),
    e("🥓", "bacon", "breakfast pork"),
    e("🥞", "pancakes", "breakfast syrup"),
    e("🍗", "poultry leg", "chicken drumstick"),
    e("🌭", "hot dog", "sausage"),
    e("🍔", "hamburger", "burger fast food"),
    e("🍟", "french fries", "fries fast food"),
    e("🍕", "pizza", "slice cheese"),
    e("🥪", "sandwich", "lunch"),
    e("🌮", "taco", "mexican"),
    e("🌯", "burrito", "mexican wrap"),
    e("🥗", "green salad", "healthy"),
    e("🍝", "spaghetti", "pasta noodles italian"),
    e("🍜", "steaming bowl", "ramen noodles soup"),
    e("🍣", "sushi", "japanese fish"),
    e("🍱", "bento box", "japanese lunch"),
    e("🍚", "cooked rice", "rice"),
    e("🍤", "fried shrimp", "tempura"),
    e("🍦", "soft ice cream", "dessert cone"),
    e("🍰", "shortcake", "cake dessert slice"),
    e("🎂", "birthday cake", "cake celebration"),
    e("🧁", "cupcake", "dessert"),
    e("🍩", "doughnut", "donut dessert"),
    e("🍪", "cookie", "dessert biscuit"),
    e("🍫", "chocolate bar", "candy dessert"),
    e("🍬", "candy", "sweet"),
    e("🍭", "lollipop", "candy sweet"),
    e("🍿", "popcorn", "movie snack"),
    e("☕", "hot beverage", "coffee tea cup"),
    e("🍵", "teacup without handle", "tea green"),
    e("🥤", "cup with straw", "soda drink"),
    e("🍺", "beer mug", "beer drink"),
    e("🍻", "clinking beer mugs", "cheers beer"),
    e("🍷", "wine glass", "wine drink"),
    e("🥂", "clinking glasses", "cheers champagne toast"),
    e("🍸", "cocktail glass", "martini drink"),
    e("🍾", "bottle with popping cork", "champagne celebrate"),
    // --- weather and nature ---
    e("☀️", "sun", "sunny weather"),
    e("⛅", "sun behind cloud", "partly cloudy"),
    e("☁️", "cloud", "cloudy overcast"),
    e("🌧️", "cloud with rain", "rain rainy weather"),
    e("⛈️", "cloud with lightning and rain", "storm thunderstorm"),
    e("🌨️", "cloud with snow", "snow snowy weather"),
    e("❄️", "snowflake", "snow winter cold"),
    e("⛄", "snowman", "winter snow"),
    e("🌪️", "tornado", "twister storm"),
    e("🌫️", "fog", "mist haze"),
    e("🌈", "rainbow", "pride colorful"),
    e("☂️", "umbrella", "rain"),
    e("⚡", "high voltage", "lightning bolt electric zap"),
    e("🌊", "water wave", "ocean sea surf"),
    e("💧", "droplet", "water drop"),
    e("🌙", "crescent moon", "night"),
    e("🌕", "full moon", "night"),
    e("⭐", "star", "favorite night"),
    e("🌟", "glowing star", "shining sparkle"),
    e("☄️", "comet", "space"),
    e("🌍", "globe showing europe africa", "earth world planet"),
    e("🪐", "ringed planet", "saturn space"),
    e("🌸", "cherry blossom", "flower spring pink"),
    e("🌹", "rose", "flower love"),
    e("🌻", "sunflower", "flower"),
    e("🌵", "cactus", "desert plant"),
    e("🌲", "evergreen tree", "pine"),
    e("🍀", "four leaf clover", "luck irish"),
    e("🍁", "maple leaf", "canada autumn fall"),
    // --- objects ---
    e("🔥", "fire", "flame hot lit burn"),
    e("🚀", "rocket", "launch ship space fast"),
    e("✨", "sparkles", "shiny magic clean new"),
    e("💥", "collision", "boom explosion bang"),
    e("💫", "dizzy", "stars sparkle"),
    e("💦", "sweat droplets", "splash water"),
    e("💨", "dashing away", "wind fast smoke"),
    e(
        "🎉",
        "party popper",
        "celebration congratulations tada party",
    ),
    e("🎊", "confetti ball", "celebration party"),
    e("🎈", "balloon", "party birthday"),
    e("🎁", "wrapped gift", "present birthday"),
    e("🏆", "trophy", "winner award champion"),
    e("🥇", "first place medal", "gold winner"),
    e("🥈", "second place medal", "silver"),
    e("🥉", "third place medal", "bronze"),
    e("⚽", "soccer ball", "football sport"),
    e("🏀", "basketball", "sport"),
    e("🏈", "american football", "sport"),
    e("⚾", "baseball", "sport"),
    e("🎾", "tennis", "sport"),
    e("🎮", "video game", "controller gaming"),
    e("🎲", "game die", "dice random"),
    e("🎯", "bullseye", "dart target goal"),
    e("🎸", "guitar", "music rock"),
    e("🎹", "musical keyboard", "piano music"),
    e("🎤", "microphone", "sing karaoke"),
    e("🎧", "headphone", "music audio"),
    e("🎬", "clapper board", "movie film"),
    e("📱", "mobile phone", "iphone smartphone"),
    e("💻", "laptop", "computer macbook"),
    e("⌨️", "keyboard", "typing"),
    e("🖥️", "desktop computer", "imac monitor"),
    e("🖨️", "printer", "print"),
    e("🖱️", "computer mouse", "click"),
    e("💾", "floppy disk", "save"),
    e("📷", "camera", "photo picture"),
    e("📺", "television", "tv screen"),
    e("⏰", "alarm clock", "time wake"),
    e("⌚", "watch", "time wrist"),
    e("⏳", "hourglass not done", "time waiting"),
    e("💡", "light bulb", "idea bright"),
    e("🔦", "flashlight", "torch light"),
    e("🔋", "battery", "power charge"),
    e("🔌", "electric plug", "power outlet"),
    e("📚", "books", "reading library study"),
    e("📖", "open book", "reading"),
    e("✏️", "pencil", "write draw"),
    e("🖊️", "pen", "write"),
    e("📝", "memo", "note write document"),
    e("📌", "pushpin", "pin location"),
    e("📎", "paperclip", "attach clip"),
    e("✂️", "scissors", "cut"),
    e("🔒", "locked", "lock secure private"),
    e("🔓", "unlocked", "open unlock"),
    e("🔑", "key", "password unlock"),
    e("🔨", "hammer", "tool build"),
    e("🛠️", "hammer and wrench", "tools settings fix"),
    e("⚙️", "gear", "settings config cog"),
    e("🧲", "magnet", "attract"),
    e("💣", "bomb", "explosive"),
    e("🔪", "kitchen knife", "knife cut"),
    e("🚗", "automobile", "car vehicle drive"),
    e("🚕", "taxi", "cab car"),
    e("🚌", "bus", "vehicle transit"),
    e("🚑", "ambulance", "emergency medical"),
    e("🚒", "fire engine", "firetruck emergency"),
    e("🚲", "bicycle", "bike cycle"),
    e("✈️", "airplane", "flight travel plane"),
    e("🚁", "helicopter", "chopper"),
    e("🚢", "ship", "boat cruise"),
    e("⛵", "sailboat", "sail boat"),
    e("🏠", "house", "home building"),
    e("🏢", "office building", "work"),
    e("🏥", "hospital", "medical building"),
    e("💰", "money bag", "cash rich dollar"),
    e("💵", "dollar banknote", "money cash usd"),
    e("💳", "credit card", "payment"),
    e("💎", "gem stone", "diamond jewel"),
    e("✅", "check mark button", "yes done complete green tick"),
    e("❌", "cross mark", "no wrong delete x red"),
    e("❎", "cross mark button", "no cancel"),
    e("⚠️", "warning", "caution alert danger"),
    e("❗", "red exclamation mark", "alert important bang"),
    e("❓", "red question mark", "question help"),
    e("💯", "hundred points", "perfect score"),
    e("🔔", "bell", "notification ring"),
    e("📣", "megaphone", "announce shout"),
    e("🏁", "chequered flag", "finish race checkered"),
    e("🚩", "triangular flag", "red flag warning marker"),
    // --- flags, major locales only ---
    e("🇺🇸", "flag united states", "usa america us"),
    e("🇬🇧", "flag united kingdom", "uk britain gb england"),
    e("🇨🇦", "flag canada", "ca"),
    e("🇲🇽", "flag mexico", "mx"),
    e("🇧🇷", "flag brazil", "br"),
    e("🇦🇷", "flag argentina", "ar"),
    e("🇫🇷", "flag france", "fr"),
    e("🇩🇪", "flag germany", "de deutschland"),
    e("🇪🇸", "flag spain", "es"),
    e("🇮🇹", "flag italy", "it"),
    e("🇵🇹", "flag portugal", "pt"),
    e("🇳🇱", "flag netherlands", "nl holland dutch"),
    e("🇨🇭", "flag switzerland", "ch swiss"),
    e("🇸🇪", "flag sweden", "se"),
    e("🇳🇴", "flag norway", "no"),
    e("🇩🇰", "flag denmark", "dk"),
    e("🇫🇮", "flag finland", "fi"),
    e("🇵🇱", "flag poland", "pl"),
    e("🇺🇦", "flag ukraine", "ua"),
    e("🇷🇺", "flag russia", "ru"),
    e("🇹🇷", "flag turkey", "tr"),
    e("🇬🇷", "flag greece", "gr"),
    e("🇮🇪", "flag ireland", "ie"),
    e("🇮🇳", "flag india", "in"),
    e("🇨🇳", "flag china", "cn"),
    e("🇯🇵", "flag japan", "jp"),
    e("🇰🇷", "flag south korea", "kr korea"),
    e("🇦🇺", "flag australia", "au"),
    e("🇳🇿", "flag new zealand", "nz"),
    e("🇿🇦", "flag south africa", "za"),
    e("🇳🇬", "flag nigeria", "ng"),
    e("🇪🇬", "flag egypt", "eg"),
    e("🇮🇱", "flag israel", "il"),
    e("🇸🇦", "flag saudi arabia", "sa"),
    e("🇦🇪", "flag united arab emirates", "ae uae dubai"),
    e("🇸🇬", "flag singapore", "sg"),
    e("🇭🇰", "flag hong kong", "hk"),
    e("🇹🇼", "flag taiwan", "tw"),
    e("🇹🇭", "flag thailand", "th"),
    e("🇻🇳", "flag vietnam", "vn"),
    e("🇮🇩", "flag indonesia", "id"),
    e("🇪🇺", "flag european union", "eu europe"),
    e("🏳️‍🌈", "rainbow flag", "pride lgbt"),
    // --- symbols: arrows ---
    e("←", "leftwards arrow", "arrow left"),
    e("→", "rightwards arrow", "arrow right"),
    e("↑", "upwards arrow", "arrow up"),
    e("↓", "downwards arrow", "arrow down"),
    e("↔", "left right arrow", "arrow horizontal"),
    e("↕", "up down arrow", "arrow vertical"),
    e("↖", "up left arrow", "arrow diagonal"),
    e("↗", "up right arrow", "arrow diagonal"),
    e("↘", "down right arrow", "arrow diagonal"),
    e("↙", "down left arrow", "arrow diagonal"),
    e("⇐", "leftwards double arrow", "arrow implied"),
    e("⇒", "rightwards double arrow", "arrow implies"),
    e("↩", "leftwards arrow with hook", "return arrow undo"),
    e("↪", "rightwards arrow with hook", "arrow redo"),
    e("⏎", "return symbol", "enter newline key"),
    // --- symbols: math ---
    e("±", "plus minus sign", "math"),
    e("×", "multiplication sign", "times math multiply"),
    e("÷", "division sign", "divide math"),
    e("≠", "not equal to", "math inequality"),
    e("≈", "almost equal to", "math approximately"),
    e("≤", "less than or equal to", "math"),
    e("≥", "greater than or equal to", "math"),
    e("∞", "infinity", "math forever"),
    e("√", "square root", "math radical"),
    e("∑", "summation", "math sum sigma"),
    e("∫", "integral", "math calculus"),
    e("∂", "partial differential", "math calculus"),
    e("∆", "increment", "math delta triangle"),
    e("π", "pi", "math greek circle"),
    e("µ", "micro sign", "mu unit greek"),
    e("°", "degree sign", "degrees temperature angle"),
    e("∈", "element of", "math set"),
    e("∪", "union", "math set"),
    e("∩", "intersection", "math set"),
    e("⊂", "subset of", "math set"),
    e("∀", "for all", "math logic"),
    e("∃", "there exists", "math logic"),
    e("¬", "not sign", "math logic negation"),
    e("∧", "logical and", "math logic"),
    e("∨", "logical or", "math logic"),
    e("⊕", "circled plus", "math xor direct sum"),
    e("∅", "empty set", "math null"),
    e("ℝ", "double struck r", "math real numbers"),
    e("ℤ", "double struck z", "math integers"),
    e("ℕ", "double struck n", "math natural numbers"),
    // --- symbols: box drawing ---
    e("┌", "box drawing corner top left", "box drawing"),
    e("┐", "box drawing corner top right", "box drawing"),
    e("└", "box drawing corner bottom left", "box drawing"),
    e("┘", "box drawing corner bottom right", "box drawing"),
    e("├", "box drawing tee left", "box drawing branch"),
    e("┤", "box drawing tee right", "box drawing"),
    e("┬", "box drawing tee top", "box drawing"),
    e("┴", "box drawing tee bottom", "box drawing"),
    e("┼", "box drawing cross", "box drawing"),
    e("─", "box drawing horizontal line", "box drawing"),
    e("│", "box drawing vertical line", "box drawing"),
    // --- symbols: quotes, dashes, punctuation ---
    e("“", "left double quotation mark", "curly quote smart"),
    e("”", "right double quotation mark", "curly quote smart"),
    e("‘", "left single quotation mark", "curly quote smart"),
    e(
        "’",
        "right single quotation mark",
        "curly quote apostrophe smart",
    ),
    e("«", "left guillemet", "quote angle double"),
    e("»", "right guillemet", "quote angle double"),
    e("•", "bullet", "list dot point"),
    e("…", "horizontal ellipsis", "dots"),
    e("—", "em dash", "dash punctuation long"),
    e("–", "en dash", "dash punctuation range"),
    e("†", "dagger", "footnote cross"),
    e("‡", "double dagger", "footnote"),
    e("§", "section sign", "legal paragraph"),
    e("¶", "pilcrow sign", "paragraph"),
    e("©", "copyright sign", "legal"),
    e("®", "registered sign", "legal trademark"),
    e("™", "trade mark sign", "legal tm"),
    // --- symbols: currency ---
    e("€", "euro sign", "currency money"),
    e("£", "pound sign", "currency money sterling"),
    e("¥", "yen sign", "currency money yuan"),
    e("¢", "cent sign", "currency money"),
    e("₹", "indian rupee sign", "currency money"),
    e("₽", "ruble sign", "currency money"),
    e("₩", "won sign", "currency money"),
    e("₿", "bitcoin sign", "currency money crypto"),
    // --- symbols: misc and mac keyboard ---
    e("✓", "check mark", "tick yes done"),
    e("✗", "ballot x", "cross no"),
    e("★", "black star", "star filled favorite"),
    e("☆", "white star", "star outline"),
    e("♠", "spade suit", "cards"),
    e("♥", "heart suit", "cards"),
    e("♦", "diamond suit", "cards"),
    e("♣", "club suit", "cards"),
    e("♪", "eighth note", "music"),
    e("♫", "beamed eighth notes", "music"),
    e("⌘", "command key", "mac cmd keyboard"),
    e("⌥", "option key", "mac alt keyboard"),
    e("⇧", "shift key", "mac keyboard"),
    e("⌃", "control key", "mac ctrl keyboard"),
    e("⎋", "escape key", "mac esc keyboard"),
    e("⌫", "delete key", "mac backspace keyboard"),
    e("⇥", "tab key", "mac keyboard"),
];

/// Search the table. An empty (or whitespace-only) query returns the
/// stable curated head of the table: the most-used smileys first. A
/// non-empty query is lowercased and matched against names and keywords;
/// results are ordered by score descending, ties by table order, and
/// capped at `limit`.
pub fn search(query: &str, limit: usize) -> Vec<&'static EmojiEntry> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return TABLE.iter().take(limit).collect();
    }
    let mut scored: Vec<(i64, usize)> = Vec::new();
    for (index, entry) in TABLE.iter().enumerate() {
        let s = score(&q, entry);
        if s > 0 {
            scored.push((s, index));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, index)| &TABLE[index])
        .collect()
}

/// Deterministic integer score for one entry: the best name signal plus
/// the best keyword signal. Zero means no match at all.
fn score(q: &str, entry: &EmojiEntry) -> i64 {
    let name = entry.name;
    let name_component = if name == q {
        NAME_EXACT
    } else if name.starts_with(q) {
        NAME_PREFIX
    } else if name.split(' ').any(|w| w.starts_with(q)) {
        NAME_WORD_PREFIX
    } else if name.contains(q) {
        NAME_SUBSTRING
    } else {
        0
    };
    let kw = entry.keywords;
    let kw_component = if kw.split(' ').any(|w| w == q) {
        KW_EXACT
    } else if kw.split(' ').any(|w| w.starts_with(q)) {
        KW_PREFIX
    } else if kw.contains(q) {
        KW_SUBSTRING
    } else {
        0
    };
    name_component + kw_component
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn glyphs(results: &[&EmojiEntry]) -> Vec<&'static str> {
        results.iter().map(|r| r.glyph).collect()
    }

    #[test]
    fn table_is_curated_not_exhaustive() {
        assert!(
            (300..=500).contains(&TABLE.len()),
            "table has {} entries; curation target is 300 to 500",
            TABLE.len()
        );
    }

    #[test]
    fn table_sanity_no_empty_fields() {
        for entry in TABLE {
            assert!(!entry.glyph.is_empty(), "empty glyph for {:?}", entry.name);
            assert!(!entry.name.is_empty(), "empty name for {:?}", entry.glyph);
            assert!(
                !entry.keywords.is_empty(),
                "empty keywords for {:?}",
                entry.name
            );
        }
    }

    #[test]
    fn table_sanity_names_and_keywords_are_lowercase_ascii() {
        for entry in TABLE {
            for field in [entry.name, entry.keywords] {
                assert!(
                    field.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == ' ' || c == '-'
                    }),
                    "non lowercase-ascii text {:?} for {:?}",
                    field,
                    entry.glyph
                );
            }
        }
    }

    #[test]
    fn table_sanity_no_duplicate_glyphs() {
        let mut seen = HashSet::new();
        for entry in TABLE {
            assert!(
                seen.insert(entry.glyph),
                "duplicate glyph {:?} ({})",
                entry.glyph,
                entry.name
            );
        }
    }

    #[test]
    fn golden_fire_is_first() {
        let results = search("fire", 5);
        assert_eq!(results[0].glyph, "\u{1f525}");
        assert_eq!(results[0].name, "fire");
    }

    #[test]
    fn golden_rocket_is_first() {
        let results = search("rocket", 5);
        assert_eq!(results[0].glyph, "\u{1f680}");
    }

    #[test]
    fn golden_check_prefers_the_green_button() {
        let results = search("check", 5);
        assert_eq!(results[0].glyph, "\u{2705}");
        // The plain check mark symbol is still findable.
        assert!(glyphs(&results).contains(&"\u{2713}"));
    }

    #[test]
    fn golden_heart_prefers_the_red_heart() {
        let results = search("heart", 10);
        assert_eq!(results[0].glyph, "\u{2764}\u{fe0f}");
        // Other hearts follow, not smileys.
        assert!(glyphs(&results).contains(&"\u{1f494}"));
    }

    #[test]
    fn keywords_reach_entries_whose_name_differs() {
        // "rofl" is only a keyword, never a name.
        let results = search("rofl", 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].glyph, "\u{1f923}");
        // "tada" finds the party popper.
        let results = search("tada", 5);
        assert_eq!(results[0].glyph, "\u{1f389}");
    }

    #[test]
    fn empty_query_returns_the_stable_smiley_head() {
        let head = search("", 5);
        assert_eq!(head.len(), 5);
        // The head is exactly the top of the table, in table order.
        for (result, expected) in head.iter().zip(TABLE.iter()) {
            assert_eq!(result.glyph, expected.glyph);
        }
        assert_eq!(head[0].glyph, "\u{1f602}");
        // Whitespace-only behaves like empty.
        assert_eq!(glyphs(&search("   ", 5)), glyphs(&search("", 5)));
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(search("", 3).len(), 3);
        assert_eq!(search("a", 1).len(), 1);
        assert!(search("arrow", 4).len() <= 4);
        assert!(search("", 0).is_empty());
        // A limit past the table length returns everything that matches.
        assert_eq!(search("", 100_000).len(), TABLE.len());
    }

    #[test]
    fn no_match_returns_empty() {
        assert!(search("zzzzqqqq", 10).is_empty());
    }

    #[test]
    fn query_is_case_and_whitespace_forgiving() {
        assert_eq!(glyphs(&search("FIRE", 3)), glyphs(&search("fire", 3)));
        assert_eq!(glyphs(&search("  fire ", 3)), glyphs(&search("fire", 3)));
    }

    #[test]
    fn symbols_are_searchable() {
        assert_eq!(search("em dash", 3)[0].glyph, "\u{2014}");
        assert_eq!(search("en dash", 3)[0].glyph, "\u{2013}");
        assert_eq!(search("command", 3)[0].glyph, "\u{2318}");
        assert_eq!(search("degree", 3)[0].glyph, "\u{b0}");
        assert!(!search("box drawing", 20).is_empty());
        assert!(!search("guillemet", 3).is_empty());
    }

    #[test]
    fn search_is_deterministic() {
        for query in ["", "fire", "heart", "arrow", "a", "flag"] {
            let a = glyphs(&search(query, 25));
            let b = glyphs(&search(query, 25));
            assert_eq!(a, b, "query {query:?} must be stable");
        }
    }
}
