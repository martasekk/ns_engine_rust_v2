//! A hundred labelled turns, so κ can be computed against something (M8 T2.7).
//!
//! Every number M8 has produced so far rests on n = 12 (the paraphrase corpus)
//! or n = 20 (the one session left in `ns.sqlite`). T2.7 gates an evaluator on
//! `evaluator_min_kappa = 0.4`, and κ is a statistic: asked over twenty turns
//! it cannot tell 0.4 from 0.1. The sample-size treatment for κ intervals
//! (Donner & Eliasziw 1992) puts **n ≈ 96** on a 95% interval of ±0.2 at a
//! positive rate near 20% — and ±0.2 is exactly the resolution the question
//! "is this above 0.4" needs. Hence a hundred.
//!
//! **The corpus is labelled, and that is the point.** M6 §8.5 calibrates an
//! evaluator against *symbolic proxies*, which is agreement between two things
//! whose accuracy is unknown. With gold labels there are three numbers instead
//! of one — κ(symbolic, gold), κ(local, gold), κ(local, symbolic) — and only
//! the first two make the third readable. A local scorer that disagrees with
//! the proxies might be wrong; it might also be right where the proxies are
//! blind, which is precisely the cross-lingual re-ask T2.1 could not reach.
//! Without gold, those two stories are indistinguishable.
//!
//! **Dev and held-out are not decoration.** The 2026 cross-dataset audit of
//! attribution metrics (`docs/research/2026-09-09-local-evaluator-findings.md`
//! §1) found that no automatic scorer transfers without target-dataset
//! validation — a clean MNLI scorer runs 0.904 AUROC on one dataset and 0.531,
//! chance, on another, and picking the best-on-average metric carried 0.172
//! AUROC of regret. Any cut-off, weight or `k` this engine chooses must be
//! chosen on [`Split::Dev`] and reported on [`Split::Held`], or the number
//! reported is the number that was fitted.
//!
//! **What it is not.** Not a sample of real traffic: it is near-balanced
//! between clean turns and failures, because that is what makes κ estimable at
//! this n, and live traffic will be nothing like balanced. The corpus κ
//! measures a scorer's **discrimination**; a live-pass κ measures agreement in
//! situ under real prevalence. Printing one and meaning the other is the
//! paradox `2026-09-09-symbolic-evaluation-findings.md` §3 warns about.
//! Not training data either — fitting anything on it would consume the only
//! held-out set there is.

/// Which half a case belongs to. Fixed in the corpus rather than sampled at
/// run time, so two runs of the same evaluator report the same number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    /// Where thresholds, weights and `k` may be chosen.
    Dev,
    /// Where the number that gets reported comes from. Nothing may be tuned
    /// by looking at these.
    Held,
}

/// The label vocabulary is [`nsevolution::evaluate::Issue`] itself, not a copy
/// of it.
///
/// A corpus with its own enum would let the two drift, and the first thing to
/// drift would be what a label *means* — at which point the agreement it
/// reports is between two vocabularies rather than between two raters.
pub use nsevolution::evaluate::Issue;

/// One graded turn, reduced to what an evaluator of any kind can see.
///
/// Deliberately not a `TurnView` from the spec: this carries no window and no
/// summary, because every case is written to be judgeable from the turn alone.
/// A corpus whose labels depend on twenty turns of prior context would be
/// measuring context assembly, not scoring.
#[derive(Debug, Clone, Copy)]
pub struct GradedTurn {
    pub id: &'static str,
    /// "cs" or "en". Both, in equal measure: the engine's traffic is Czech,
    /// and a corpus that reported an English number would flatter every
    /// scorer here — including the lexical one, whose worst case is Czech
    /// inflection.
    pub lang: &'static str,
    pub split: Split,
    /// What the router decided the turn was for. `true` for the tier that
    /// means "something is to be done", which is what `IgnoredRequest` reads.
    pub task_tier: bool,
    pub user: &'static str,
    /// The material the replier was shown — facts and the current turn's
    /// trace, in the replier's own rendering. What "grounded" is judged
    /// against.
    pub shown: &'static [&'static str],
    /// Whether the turn called a tool or proposed a real action.
    pub acted: bool,
    pub reply: &'static str,
    /// The user's next message, where there is one. `Reask` and `Correction`
    /// are visible only here — they are properties of what happened next.
    pub next_user: Option<&'static str>,
    pub gold: Issue,
}

macro_rules! case {
    (
        $id:expr, $lang:expr, $split:ident, task: $task:expr, acted: $acted:expr,
        user: $user:expr,
        shown: $shown:expr,
        reply: $reply:expr,
        next: $next:expr,
        gold: $gold:ident $(,)?
    ) => {
        GradedTurn {
            id: $id,
            lang: $lang,
            split: Split::$split,
            task_tier: $task,
            user: $user,
            shown: $shown,
            acted: $acted,
            reply: $reply,
            next_user: $next,
            gold: Issue::$gold,
        }
    };
}

/// The hundred. Forty clean, sixty failures across five kinds, half Czech,
/// half English, alternating development and held-out within every kind so
/// neither half is easier than the other.
pub fn corpus() -> &'static [GradedTurn] {
    &CORPUS
}

const CORPUS: [GradedTurn; 100] = [
    // ---- Clean, English (20) --------------------------------------------
    case!("en/clean/01", "en", Dev, task: false, acted: false,
        user: "what time is it?",
        shown: &["did: get_time -> 09:36 UTC, Tuesday 2026-09-08"],
        reply: "It is 09:36 UTC on Tuesday, 2026-09-08.",
        next: Some("thanks"),
        gold: None),
    case!("en/clean/02", "en", Held, task: true, acted: true,
        user: "move the pointer to the centre of the screen",
        shown: &["did: pointer_screens -> 1920x1080", "did: pointer_move -> 960,540"],
        reply: "The screen is 1920 by 1080, so I moved the pointer to 960, 540.",
        next: Some("good"),
        gold: None),
    case!("en/clean/03", "en", Dev, task: false, acted: false,
        user: "what is our budget for the campaign?",
        shown: &["budget.campaign: 2000 crowns"],
        reply: "The campaign budget is 2000 crowns.",
        next: Some("right, and the deadline?"),
        gold: None),
    case!("en/clean/04", "en", Held, task: false, acted: false,
        user: "what is my name?",
        shown: &["user.name: Martin"],
        reply: "Your name is Martin.",
        next: Some("correct"),
        gold: None),
    case!("en/clean/05", "en", Dev, task: true, acted: true,
        user: "close the Edge window",
        shown: &["did: pointer_ui_read -> Edge window", "did: pointer_ui_find -> close button at 389,1056", "did: pointer_click -> ok"],
        reply: "I clicked the close button on the Edge window.",
        next: Some("thanks, now open firefox"),
        gold: None),
    case!("en/clean/06", "en", Held, task: false, acted: false,
        user: "how should you write clock values for me?",
        shown: &["pref.time_format: 24-hour, never am/pm"],
        reply: "In 24-hour form, never am or pm.",
        next: Some("yes"),
        gold: None),
    case!("en/clean/07", "en", Dev, task: true, acted: true,
        user: "empty the recycle bin",
        shown: &["did: pointer_ui_find -> Recycle Bin", "did: pointer_click -> Empty Recycle Bin"],
        reply: "The recycle bin is empty. I opened its menu and confirmed the emptying.",
        next: Some("perfect"),
        gold: None),
    case!("en/clean/08", "en", Held, task: false, acted: false,
        user: "when is the fair?",
        shown: &["deadline.fair: the twelfth"],
        reply: "The fair is on the twelfth, and everything has to be finished before it.",
        next: Some("noted"),
        gold: None),
    case!("en/clean/09", "en", Dev, task: false, acted: false,
        user: "which window is the invoice one?",
        shown: &["desktop.invoices: the window titled Faktury on the second screen"],
        reply: "The one titled Faktury, on your second screen.",
        next: Some("got it"),
        gold: None),
    case!("en/clean/10", "en", Held, task: true, acted: true,
        user: "type the order number into the search box",
        shown: &["did: pointer_click -> search box", "did: pointer_type -> 88214"],
        reply: "I clicked the search box and typed 88214 into it.",
        next: Some("thanks"),
        gold: None),
    case!("en/clean/11", "en", Dev, task: false, acted: false,
        user: "what am I not allowed to do on my own?",
        shown: &["rule.clients: do not send anything to clients without showing me first"],
        reply: "Send anything to clients without showing you first.",
        next: Some("exactly"),
        gold: None),
    case!("en/clean/12", "en", Held, task: false, acted: false,
        user: "is the printer set up?",
        shown: &["did: check_printer -> queue empty, driver ok"],
        reply: "Yes — the driver is installed and the queue is empty.",
        next: Some("good, print the flyers then"),
        gold: None),
    case!("en/clean/13", "en", Dev, task: true, acted: true,
        user: "scroll down in the invoice list",
        shown: &["did: pointer_scroll -> 3 notches down"],
        reply: "I scrolled the invoice list down three notches.",
        next: Some("a bit more"),
        gold: None),
    case!("en/clean/14", "en", Held, task: false, acted: false,
        user: "how many flyers did we say?",
        shown: &["order.flyers: two hundred"],
        reply: "Two hundred flyers.",
        next: Some("yes"),
        gold: None),
    case!("en/clean/15", "en", Dev, task: false, acted: false,
        user: "can you remind me what the supplier said?",
        shown: &["note.supplier: paper arrives Thursday, one pallet short"],
        reply: "The paper arrives Thursday, and they are one pallet short.",
        next: Some("call them about the pallet"),
        gold: None),
    case!("en/clean/16", "en", Held, task: true, acted: true,
        user: "open the browser",
        shown: &["did: pointer_ui_find -> Firefox icon", "did: pointer_click -> ok"],
        reply: "Firefox is open.",
        next: Some("go to the supplier site"),
        gold: None),
    case!("en/clean/17", "en", Dev, task: false, acted: false,
        user: "what did I book for the stand?",
        shown: &["note.stand: stand B12, six square metres"],
        reply: "Stand B12, six square metres.",
        next: Some("that's the one"),
        gold: None),
    case!("en/clean/18", "en", Held, task: false, acted: false,
        user: "do I have anything due today?",
        shown: &["did: list_tasks -> nothing due before the twelfth"],
        reply: "Nothing is due today — the next thing falls on the twelfth.",
        next: Some("good"),
        gold: None),
    case!("en/clean/19", "en", Dev, task: true, acted: true,
        user: "take a screenshot of the second monitor",
        shown: &["did: pointer_screens -> 2 screens", "did: pointer_shot -> screen 2 captured"],
        reply: "I captured the second screen.",
        next: Some("thanks"),
        gold: None),
    case!("en/clean/20", "en", Held, task: false, acted: false,
        user: "which of the two quotes was cheaper?",
        shown: &["quote.a: 4100 crowns", "quote.b: 3850 crowns"],
        reply: "The second one, at 3850 crowns against 4100.",
        next: Some("take that one"),
        gold: None),
    // ---- Clean, Czech (20) ----------------------------------------------
    case!("cs/clean/01", "cs", Dev, task: false, acted: false,
        user: "kolik je hodin?",
        shown: &["did: get_time -> 09:36 UTC, úterý 2026-09-08"],
        reply: "Je 09:36 UTC, úterý 2026-09-08.",
        next: Some("díky"),
        gold: None),
    case!("cs/clean/02", "cs", Held, task: true, acted: true,
        user: "posuň kurzor doprostřed obrazovky",
        shown: &["did: pointer_screens -> 1920x1080", "did: pointer_move -> 960,540"],
        reply: "Obrazovka má 1920 na 1080, kurzor jsem posunul na 960, 540.",
        next: Some("dobře"),
        gold: None),
    case!("cs/clean/03", "cs", Dev, task: false, acted: false,
        user: "jaký máme rozpočet na kampaň?",
        shown: &["budget.campaign: 2000 korun"],
        reply: "Rozpočet na kampaň je 2000 korun.",
        next: Some("a termín?"),
        gold: None),
    case!("cs/clean/04", "cs", Held, task: false, acted: false,
        user: "jak se jmenuju?",
        shown: &["user.name: Martin"],
        reply: "Jmenujete se Martin.",
        next: Some("přesně tak"),
        gold: None),
    case!("cs/clean/05", "cs", Dev, task: true, acted: true,
        user: "zavři okno Edge",
        shown: &["did: pointer_ui_read -> okno Edge", "did: pointer_ui_find -> křížek na 389,1056", "did: pointer_click -> ok"],
        reply: "Klikl jsem na křížek okna Edge, okno je zavřené.",
        next: Some("díky, teď otevři firefox"),
        gold: None),
    case!("cs/clean/06", "cs", Held, task: false, acted: false,
        user: "jak mi máš psát časy?",
        shown: &["pref.time_format: 24 hodin, nikdy am/pm"],
        reply: "Ve formátu 24 hodin, nikdy am ani pm.",
        next: Some("ano"),
        gold: None),
    case!("cs/clean/07", "cs", Dev, task: true, acted: true,
        user: "vysyp koš",
        shown: &["did: pointer_ui_find -> Koš", "did: pointer_click -> Vysypat koš"],
        reply: "Koš je vysypaný. Otevřel jsem nabídku a potvrdil vysypání.",
        next: Some("super"),
        gold: None),
    case!("cs/clean/08", "cs", Held, task: false, acted: false,
        user: "kdy je ten veletrh?",
        shown: &["deadline.fair: dvanáctého"],
        reply: "Veletrh je dvanáctého a všechno musí být hotové před ním.",
        next: Some("jasně"),
        gold: None),
    case!("cs/clean/09", "cs", Dev, task: false, acted: false,
        user: "které okno je to s fakturami?",
        shown: &["desktop.invoices: okno Faktury na druhé obrazovce"],
        reply: "To s názvem Faktury, na druhé obrazovce.",
        next: Some("aha, dobře"),
        gold: None),
    case!("cs/clean/10", "cs", Held, task: true, acted: true,
        user: "napiš číslo objednávky do vyhledávání",
        shown: &["did: pointer_click -> vyhledávací pole", "did: pointer_type -> 88214"],
        reply: "Kliknul jsem do vyhledávacího pole a napsal 88214.",
        next: Some("díky"),
        gold: None),
    case!("cs/clean/11", "cs", Dev, task: false, acted: false,
        user: "co nesmím dělat sám?",
        shown: &["rule.clients: nic neposílat klientům bez toho, abys mi to ukázal"],
        reply: "Posílat cokoli klientům, aniž bych vám to nejdřív ukázal.",
        next: Some("tak"),
        gold: None),
    case!("cs/clean/12", "cs", Held, task: false, acted: false,
        user: "je tiskárna nachystaná?",
        shown: &["did: check_printer -> fronta prázdná, ovladač ok"],
        reply: "Ano, ovladač je nainstalovaný a fronta je prázdná.",
        next: Some("dobře, tak vytiskni letáky"),
        gold: None),
    case!("cs/clean/13", "cs", Dev, task: true, acted: true,
        user: "posuň seznam faktur dolů",
        shown: &["did: pointer_scroll -> 3 zářezy dolů"],
        reply: "Posunul jsem seznam faktur o tři zářezy dolů.",
        next: Some("ještě kousek"),
        gold: None),
    case!("cs/clean/14", "cs", Held, task: false, acted: false,
        user: "kolik letáků jsme říkali?",
        shown: &["order.flyers: dvě stě"],
        reply: "Dvě stě letáků.",
        next: Some("ano"),
        gold: None),
    case!("cs/clean/15", "cs", Dev, task: false, acted: false,
        user: "co říkal dodavatel?",
        shown: &["note.supplier: papír přijde ve čtvrtek, chybí jedna paleta"],
        reply: "Papír přijde ve čtvrtek a chybí jim jedna paleta.",
        next: Some("zavolej jim kvůli té paletě"),
        gold: None),
    case!("cs/clean/16", "cs", Held, task: true, acted: true,
        user: "otevři prohlížeč",
        shown: &["did: pointer_ui_find -> ikona Firefox", "did: pointer_click -> ok"],
        reply: "Firefox je otevřený.",
        next: Some("běž na stránky dodavatele"),
        gold: None),
    case!("cs/clean/17", "cs", Dev, task: false, acted: false,
        user: "co jsem zamluvil za stánek?",
        shown: &["note.stand: stánek B12, šest metrů čtverečních"],
        reply: "Stánek B12, šest metrů čtverečních.",
        next: Some("ten je to"),
        gold: None),
    case!("cs/clean/18", "cs", Held, task: false, acted: false,
        user: "mám dneska něco termínovaného?",
        shown: &["did: list_tasks -> nic před dvanáctým"],
        reply: "Dneska nic — nejbližší termín je dvanáctého.",
        next: Some("dobře"),
        gold: None),
    case!("cs/clean/19", "cs", Dev, task: true, acted: true,
        user: "udělej snímek druhého monitoru",
        shown: &["did: pointer_screens -> 2 obrazovky", "did: pointer_shot -> obrazovka 2"],
        reply: "Snímek druhé obrazovky je hotový.",
        next: Some("díky"),
        gold: None),
    case!("cs/clean/20", "cs", Held, task: false, acted: false,
        user: "která z těch dvou nabídek byla levnější?",
        shown: &["quote.a: 4100 korun", "quote.b: 3850 korun"],
        reply: "Ta druhá, 3850 korun proti 4100.",
        next: Some("ber tu"),
        gold: None),
    // ---- Re-ask (15) -----------------------------------------------------
    // The failure is in `next_user`: the same want, restated. Half of these
    // restate it in different words, and two cross languages, which is where
    // T2.1's lexical band is blind by construction.
    case!("en/reask/01", "en", Dev, task: true, acted: true,
        user: "move my mouse to the top right",
        shown: &["did: pointer_move -> denied by repeat_gate"],
        reply: "Sorry, I couldn't complete that.",
        next: Some("move my mouse to the top right"),
        gold: Reask),
    case!("en/reask/02", "en", Held, task: true, acted: true,
        user: "can you move my mouse to the top right",
        shown: &["did: pointer_move -> ran out of steps"],
        reply: "Sorry, I couldn't complete that.",
        next: Some("hey can you move my cursor to the top right"),
        gold: Reask),
    case!("en/reask/03", "en", Dev, task: false, acted: false,
        user: "confirm",
        shown: &["did: nothing — session not armed"],
        reply: "To use mouse control you need to arm this session first.",
        next: Some("confirm"),
        gold: Reask),
    case!("en/reask/04", "en", Held, task: false, acted: false,
        user: "what is our budget?",
        shown: &["budget.campaign: 2000 crowns"],
        reply: "I don't have a budget recorded for that.",
        next: Some("how much are we allowed to spend on this"),
        gold: Reask),
    case!("en/reask/05", "en", Dev, task: true, acted: true,
        user: "empty my recycle bin",
        shown: &["did: pointer_ui_find -> no matching control"],
        reply: "I searched for the Recycle Bin but found no matching control.",
        next: Some("vysyp muj koš"),
        gold: Reask),
    case!("en/reask/06", "en", Held, task: false, acted: false,
        user: "when does everything have to be finished?",
        shown: &["deadline.fair: the twelfth"],
        reply: "There are several deadlines in play.",
        next: Some("what is the last possible day for this work"),
        gold: Reask),
    case!("en/reask/07", "en", Dev, task: true, acted: true,
        user: "close my browser",
        shown: &["did: pointer_ui_read -> Firefox and Edge open"],
        reply: "You have several browsers open.",
        next: Some("close my browser"),
        gold: Reask),
    case!("en/reask/08", "en", Held, task: false, acted: false,
        user: "who am I again?",
        shown: &["user.name: Martin"],
        reply: "I'm not sure I have that.",
        next: Some("remind me what my name is"),
        gold: Reask),
    case!("cs/reask/01", "cs", Dev, task: true, acted: true,
        user: "vysyp muj koš",
        shown: &["did: pointer_click -> WIN32_ERROR(5)"],
        reply: "Koš se nepodařilo vysypat, kliknutí selhalo.",
        next: Some("vysyp muj koš"),
        gold: Reask),
    case!("cs/reask/02", "cs", Held, task: true, acted: true,
        user: "vysyp muj koš",
        shown: &["did: pointer_ui_find -> nenalezeno"],
        reply: "Položku Koš se nepodařilo v rozhraní najít.",
        next: Some("vyprázdni mi odpadkový koš na ploše"),
        gold: Reask),
    case!("cs/reask/03", "cs", Dev, task: false, acted: false,
        user: "jaký máme rozpočet?",
        shown: &["budget.campaign: 2000 korun"],
        reply: "Žádný rozpočet u sebe nemám.",
        next: Some("kolik můžeme na tohle utratit"),
        gold: Reask),
    case!("cs/reask/04", "cs", Held, task: false, acted: false,
        user: "potvrzuji",
        shown: &["did: nothing — relace není odemčená"],
        reply: "K ovládání myši je potřeba nejdřív odemknout relaci.",
        next: Some("potvrzuji"),
        gold: Reask),
    case!("cs/reask/05", "cs", Dev, task: false, acted: false,
        user: "kdy to musí být hotové?",
        shown: &["deadline.fair: dvanáctého"],
        reply: "Termínů je tam víc.",
        next: Some("jaký je poslední možný den na tuhle práci"),
        gold: Reask),
    case!("cs/reask/06", "cs", Held, task: true, acted: true,
        user: "zavři mi prohlížeč",
        shown: &["did: pointer_ui_read -> Firefox i Edge otevřené"],
        reply: "Máte otevřených víc prohlížečů.",
        next: Some("zavři mi prohlížeč"),
        gold: Reask),
    case!("cs/reask/07", "cs", Dev, task: false, acted: false,
        user: "kdo jsem?",
        shown: &["user.name: Martin"],
        reply: "To u sebe bohužel nemám.",
        next: Some("what is my name, you had it earlier"),
        gold: Reask),
    // ---- Ignored question, I5 (15) ---------------------------------------
    // The reply is coherent and about something else. Note how many of these
    // a content-token overlap check cannot see: the reply reuses the
    // question's words while answering a different question.
    case!("en/ignoredq/01", "en", Dev, task: false, acted: false,
        user: "which browser windows are open?",
        shown: &["did: pointer_ui_read -> Firefox, Edge"],
        reply: "The recycle bin has been emptied.",
        next: Some("that isn't what I asked"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/02", "en", Held, task: false, acted: false,
        user: "how much is left in the budget?",
        shown: &["budget.campaign: 2000 crowns", "budget.spent: 1450 crowns"],
        reply: "The budget for the campaign is 2000 crowns.",
        next: Some("I asked how much is left"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/03", "en", Dev, task: false, acted: false,
        user: "when is the paper arriving?",
        shown: &["note.supplier: paper arrives Thursday, one pallet short"],
        reply: "The supplier is one pallet short.",
        next: Some("yes but when"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/04", "en", Held, task: false, acted: false,
        user: "which of the two quotes was cheaper?",
        shown: &["quote.a: 4100 crowns", "quote.b: 3850 crowns"],
        reply: "Both quotes are valid until the end of the month.",
        next: Some("cheaper, I said"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/05", "en", Dev, task: false, acted: false,
        user: "is the printer set up?",
        shown: &["did: check_printer -> queue empty, driver ok"],
        reply: "The printer is a Kyocera on the second floor.",
        next: Some("set up or not?"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/06", "en", Held, task: false, acted: false,
        user: "what time does the fair open?",
        shown: &["deadline.fair: the twelfth"],
        reply: "The fair is on the twelfth.",
        next: Some("what time though"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/07", "en", Dev, task: false, acted: false,
        user: "how many flyers are still unprinted?",
        shown: &["order.flyers: two hundred", "did: count_printed -> 120"],
        reply: "You ordered two hundred flyers.",
        next: Some("unprinted, how many"),
        gold: IgnoredQuestion),
    case!("en/ignoredq/08", "en", Held, task: false, acted: false,
        user: "did the client reply yet?",
        shown: &["did: check_mail -> nothing new"],
        reply: "Remember you asked me not to send anything to clients without showing you.",
        next: Some("I asked whether they replied"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/01", "cs", Dev, task: false, acted: false,
        user: "která okna prohlížeče jsou otevřená?",
        shown: &["did: pointer_ui_read -> Firefox, Edge"],
        reply: "Koš byl vysypán.",
        next: Some("na to jsem se neptal"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/02", "cs", Held, task: false, acted: false,
        user: "kolik zbývá v rozpočtu?",
        shown: &["budget.campaign: 2000 korun", "budget.spent: 1450 korun"],
        reply: "Rozpočet na kampaň je 2000 korun.",
        next: Some("ptal jsem se, kolik zbývá"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/03", "cs", Dev, task: false, acted: false,
        user: "kdy přijde ten papír?",
        shown: &["note.supplier: papír přijde ve čtvrtek, chybí paleta"],
        reply: "Dodavateli chybí jedna paleta.",
        next: Some("ano, ale kdy"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/04", "cs", Held, task: false, acted: false,
        user: "která z těch nabídek je levnější?",
        shown: &["quote.a: 4100 korun", "quote.b: 3850 korun"],
        reply: "Obě nabídky platí do konce měsíce.",
        next: Some("levnější, říkal jsem"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/05", "cs", Dev, task: false, acted: false,
        user: "v kolik se veletrh otevírá?",
        shown: &["deadline.fair: dvanáctého"],
        reply: "Veletrh je dvanáctého.",
        next: Some("ale v kolik"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/06", "cs", Held, task: false, acted: false,
        user: "kolik letáků ještě není vytištěných?",
        shown: &["order.flyers: dvě stě", "did: count_printed -> 120"],
        reply: "Objednal jste dvě stě letáků.",
        next: Some("nevytištěných, kolik"),
        gold: IgnoredQuestion),
    case!("cs/ignoredq/07", "cs", Dev, task: false, acted: false,
        user: "odpověděl už klient?",
        shown: &["did: check_mail -> nic nového"],
        reply: "Připomínám, že jsem nemám klientům nic posílat bez vašeho svolení.",
        next: Some("ptal jsem se, jestli odpověděl"),
        gold: IgnoredQuestion),
    // ---- Ignored request, I6 (10) ----------------------------------------
    // Task tier, nothing done, and a reply that sounds like compliance.
    case!("en/ignoredr/01", "en", Dev, task: true, acted: false,
        user: "empty my recycle bin",
        shown: &[],
        reply: "Sure, I can help with that.",
        next: Some("so do it"),
        gold: IgnoredRequest),
    case!("en/ignoredr/02", "en", Held, task: true, acted: false,
        user: "close the Edge window",
        shown: &[],
        reply: "Closing browser windows is straightforward once they are in focus.",
        next: Some("close it then"),
        gold: IgnoredRequest),
    case!("en/ignoredr/03", "en", Dev, task: true, acted: false,
        user: "open firefox and go to the supplier site",
        shown: &[],
        reply: "Firefox is a good choice for that site.",
        next: Some("open it"),
        gold: IgnoredRequest),
    case!("en/ignoredr/04", "en", Held, task: true, acted: false,
        user: "take a screenshot of the second monitor",
        shown: &[],
        reply: "Screenshots of a second monitor need the screen index.",
        next: Some("you have it, just take it"),
        gold: IgnoredRequest),
    case!("en/ignoredr/05", "en", Dev, task: true, acted: false,
        user: "type the order number into the search box",
        shown: &[],
        reply: "The search box is usually at the top of the window.",
        next: Some("type it in"),
        gold: IgnoredRequest),
    case!("cs/ignoredr/01", "cs", Held, task: true, acted: false,
        user: "vysyp mi koš",
        shown: &[],
        reply: "Jistě, s tím vám pomůžu.",
        next: Some("tak to udělej"),
        gold: IgnoredRequest),
    case!("cs/ignoredr/02", "cs", Dev, task: true, acted: false,
        user: "zavři okno Edge",
        shown: &[],
        reply: "Zavírání oken je snadné, jakmile jsou aktivní.",
        next: Some("tak ho zavři"),
        gold: IgnoredRequest),
    case!("cs/ignoredr/03", "cs", Held, task: true, acted: false,
        user: "otevři firefox a běž na stránky dodavatele",
        shown: &[],
        reply: "Firefox je na tu stránku dobrá volba.",
        next: Some("tak ho otevři"),
        gold: IgnoredRequest),
    case!("cs/ignoredr/04", "cs", Dev, task: true, acted: false,
        user: "udělej snímek druhé obrazovky",
        shown: &[],
        reply: "K snímku druhé obrazovky je potřeba její index.",
        next: Some("ten máš, tak ho udělej"),
        gold: IgnoredRequest),
    case!("cs/ignoredr/05", "cs", Held, task: true, acted: false,
        user: "napiš číslo objednávky do vyhledávání",
        shown: &[],
        reply: "Vyhledávací pole bývá nahoře v okně.",
        next: Some("napiš to tam"),
        gold: IgnoredRequest),
    // ---- Ungrounded (10) --------------------------------------------------
    // A number, a name or a quantity that nothing in `shown` supports. The
    // reply is otherwise on topic and fluent, which is the whole difficulty.
    case!("en/ungrounded/01", "en", Dev, task: false, acted: false,
        user: "what time is it?",
        shown: &["did: get_time -> failed, no clock tool available"],
        reply: "It is 14:05 in Oslo.",
        next: Some("you can't know that"),
        gold: Ungrounded),
    case!("en/ungrounded/02", "en", Held, task: false, acted: false,
        user: "how much is the second quote?",
        shown: &["quote.a: 4100 crowns"],
        reply: "The second quote came to 3850 crowns.",
        next: Some("where did you get that"),
        gold: Ungrounded),
    case!("en/ungrounded/03", "en", Dev, task: false, acted: false,
        user: "who is handling the stand?",
        shown: &["note.stand: stand B12, six square metres"],
        reply: "Stand B12 is being handled by Petra from the fair office.",
        next: Some("there is no Petra"),
        gold: Ungrounded),
    case!("en/ungrounded/04", "en", Held, task: true, acted: true,
        user: "what resolution is the second screen?",
        shown: &["did: pointer_screens -> 2 screens"],
        reply: "The second screen is 2560 by 1440.",
        next: Some("you didn't check that"),
        gold: Ungrounded),
    case!("en/ungrounded/05", "en", Dev, task: false, acted: false,
        user: "when did the supplier last deliver?",
        shown: &["note.supplier: paper arrives Thursday"],
        reply: "They last delivered on 2026-08-19.",
        next: Some("made up"),
        gold: Ungrounded),
    case!("cs/ungrounded/01", "cs", Held, task: false, acted: false,
        user: "kolik je hodin?",
        shown: &["did: get_time -> selhalo, nástroj není k dispozici"],
        reply: "Je 14:05.",
        next: Some("to nemůžeš vědět"),
        gold: Ungrounded),
    case!("cs/ungrounded/02", "cs", Dev, task: false, acted: false,
        user: "kolik stojí ta druhá nabídka?",
        shown: &["quote.a: 4100 korun"],
        reply: "Druhá nabídka vyšla na 3850 korun.",
        next: Some("odkud to máš"),
        gold: Ungrounded),
    case!("cs/ungrounded/03", "cs", Held, task: false, acted: false,
        user: "kdo má na starosti stánek?",
        shown: &["note.stand: stánek B12, šest metrů"],
        reply: "Stánek B12 má na starosti Petra z kanceláře veletrhu.",
        next: Some("žádná Petra tam není"),
        gold: Ungrounded),
    case!("cs/ungrounded/04", "cs", Dev, task: true, acted: true,
        user: "jaké rozlišení má druhá obrazovka?",
        shown: &["did: pointer_screens -> 2 obrazovky"],
        reply: "Druhá obrazovka má 2560 na 1440.",
        next: Some("to jsi nezjišťoval"),
        gold: Ungrounded),
    case!("cs/ungrounded/05", "cs", Held, task: false, acted: false,
        user: "kdy dodavatel naposledy dovezl?",
        shown: &["note.supplier: papír přijde ve čtvrtek"],
        reply: "Naposledy dovezl 2026-08-19.",
        next: Some("vymyšlené"),
        gold: Ungrounded),
    // ---- Correction (10) --------------------------------------------------
    // The reply is plausible and the *next* message says it was wrong. Only
    // the follow-up carries the signal; nothing inside the turn does.
    case!("en/correction/01", "en", Dev, task: false, acted: false,
        user: "what is my name?",
        shown: &["user.name: Martina"],
        reply: "Your name is Martina.",
        next: Some("no, it's Martin"),
        gold: Correction),
    case!("en/correction/02", "en", Held, task: false, acted: false,
        user: "how many flyers?",
        shown: &["order.flyers: two hundred"],
        reply: "Two hundred flyers.",
        next: Some("we changed it to three hundred"),
        gold: Correction),
    case!("en/correction/03", "en", Dev, task: false, acted: false,
        user: "which stand did I book?",
        shown: &["note.stand: stand B12"],
        reply: "Stand B12.",
        next: Some("it's B21 now, they moved us"),
        gold: Correction),
    case!("en/correction/04", "en", Held, task: false, acted: false,
        user: "when does the paper arrive?",
        shown: &["note.supplier: paper arrives Thursday"],
        reply: "Thursday.",
        next: Some("friday, they called this morning"),
        gold: Correction),
    case!("en/correction/05", "en", Dev, task: false, acted: false,
        user: "what is the campaign budget?",
        shown: &["budget.campaign: 2000 crowns"],
        reply: "2000 crowns.",
        next: Some("that was last month, it's 2500 now"),
        gold: Correction),
    case!("cs/correction/01", "cs", Held, task: false, acted: false,
        user: "jak se jmenuju?",
        shown: &["user.name: Martina"],
        reply: "Jmenujete se Martina.",
        next: Some("ne, Martin"),
        gold: Correction),
    case!("cs/correction/02", "cs", Dev, task: false, acted: false,
        user: "kolik letáků?",
        shown: &["order.flyers: dvě stě"],
        reply: "Dvě stě letáků.",
        next: Some("změnili jsme to na tři sta"),
        gold: Correction),
    case!("cs/correction/03", "cs", Held, task: false, acted: false,
        user: "jaký stánek jsem zamluvil?",
        shown: &["note.stand: stánek B12"],
        reply: "Stánek B12.",
        next: Some("teď je to B21, přesunuli nás"),
        gold: Correction),
    case!("cs/correction/04", "cs", Dev, task: false, acted: false,
        user: "kdy přijde papír?",
        shown: &["note.supplier: papír přijde ve čtvrtek"],
        reply: "Ve čtvrtek.",
        next: Some("v pátek, volali ráno"),
        gold: Correction),
    case!("cs/correction/05", "cs", Held, task: false, acted: false,
        user: "jaký je rozpočet kampaně?",
        shown: &["budget.campaign: 2000 korun"],
        reply: "2000 korun.",
        next: Some("to bylo minulý měsíc, teď je 2500"),
        gold: Correction),
];

// ---------------------------------------------------------------------------
// Running an evaluator over the corpus
// ---------------------------------------------------------------------------

use nsevolution::evaluate::{Evaluator, GradeError, TurnView};
use nsevolution::kappa::{Agreement, Scores};

/// One case, judged.
#[derive(Debug, Clone)]
pub struct Judged {
    pub id: &'static str,
    pub lang: &'static str,
    pub gold: Issue,
    /// What the evaluator said, or why it could not say.
    pub got: Result<Issue, GradeError>,
}

/// A whole run, kept case by case so the report can say *which* cases moved.
///
/// A single κ is a summary; the argument for or against a scorer is always in
/// the disagreements, and a run that threw them away would make every question
/// about it unanswerable without re-running.
#[derive(Debug, Clone)]
pub struct GradeRun {
    pub scorer: String,
    pub split: Option<Split>,
    pub cases: Vec<Judged>,
}

impl GradeRun {
    /// Cases the evaluator actually answered.
    pub fn answered(&self) -> impl Iterator<Item = (&Judged, Issue)> {
        self.cases
            .iter()
            .filter_map(|c| c.got.as_ref().ok().map(|i| (c, *i)))
    }
    pub fn unavailable(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c.got, Err(GradeError::Unavailable(_))))
            .count()
    }
    pub fn invalid(&self) -> usize {
        self.cases
            .iter()
            .filter(|c| matches!(c.got, Err(GradeError::Invalid(_))))
            .count()
    }

    /// Agreement on the binary the gate consumes: did the evaluator see a
    /// problem where the label says there is one.
    ///
    /// Unanswered cases are **excluded**, not counted as "no problem". A
    /// scorer that was unreachable did not say a turn was fine, and scoring
    /// its silence as agreement with every clean case would let a dead
    /// service post a respectable κ.
    pub fn against_gold(&self) -> Agreement {
        let mut got = Vec::new();
        let mut gold = Vec::new();
        for (c, i) in self.answered() {
            got.push(i.is_problem());
            gold.push(c.gold.is_problem());
        }
        Agreement::tally(&got, &gold)
    }

    /// Agreement with another run over the cases both answered — the number
    /// T2.7's gate is actually written in terms of, when the reference is a
    /// symbolic proxy rather than a label.
    pub fn against(&self, other: &GradeRun) -> Agreement {
        let mut mine = Vec::new();
        let mut theirs = Vec::new();
        for (c, i) in self.answered() {
            if let Some((_, j)) = other.answered().find(|(o, _)| o.id == c.id) {
                mine.push(i.is_problem());
                theirs.push(j.is_problem());
            }
        }
        Agreement::tally(&mine, &theirs)
    }

    /// How often each labelled kind was caught at all, whatever it was called.
    pub fn recall_by_kind(&self) -> Vec<(Issue, usize, usize)> {
        let kinds = [
            Issue::None,
            Issue::Reask,
            Issue::IgnoredQuestion,
            Issue::IgnoredRequest,
            Issue::Ungrounded,
            Issue::Correction,
        ];
        kinds
            .iter()
            .map(|&k| {
                let cases: Vec<Issue> = self
                    .answered()
                    .filter(|(c, _)| c.gold == k)
                    .map(|(_, i)| i)
                    .collect();
                let hit = cases
                    .iter()
                    .filter(|&&i| {
                        if k == Issue::None {
                            !i.is_problem()
                        } else {
                            i.is_problem()
                        }
                    })
                    .count();
                (k, hit, cases.len())
            })
            .collect()
    }

    pub fn scores(&self) -> Scores {
        self.against_gold().scores()
    }
}

impl std::fmt::Display for GradeRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let split = match self.split {
            Some(Split::Dev) => "dev",
            Some(Split::Held) => "held-out",
            None => "all",
        };
        writeln!(f, "scorer: {} ({split})", self.scorer)?;
        writeln!(
            f,
            "answered {}/{}  unavailable {}  invalid {}",
            self.cases.len() - self.unavailable() - self.invalid(),
            self.cases.len(),
            self.unavailable(),
            self.invalid()
        )?;
        writeln!(f, "caught, by labelled kind:")?;
        for (kind, hit, n) in self.recall_by_kind() {
            if n > 0 {
                writeln!(f, "  {:<18} {hit}/{n}", kind.as_str())?;
            }
        }
        write!(f, "vs gold: {}", self.scores())
    }
}

/// Grade every case in `split` (or all of them) with `ev`.
pub async fn run(ev: &dyn Evaluator, split: Option<Split>) -> GradeRun {
    let mut cases = Vec::new();
    for c in corpus()
        .iter()
        .filter(|c| split.is_none_or(|s| c.split == s))
    {
        let view = TurnView {
            user: c.user,
            shown: c.shown,
            acted: c.acted,
            task_tier: c.task_tier,
            reply: c.reply,
            next_user: c.next_user,
        };
        cases.push(Judged {
            id: c.id,
            lang: c.lang,
            gold: c.gold,
            got: ev.grade(&view).await.map(|g| g.issue),
        });
    }
    GradeRun {
        scorer: ev.id(),
        split,
        cases,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The size the statistic needs. Donner & Eliasziw put a ±0.2 interval on
    /// κ at ~96 cases; below that the gate at 0.4 cannot be resolved.
    #[test]
    fn the_corpus_is_large_enough_for_the_statistic_it_feeds() {
        assert!(
            corpus().len() >= 96,
            "n = {} is under the ~96 a ±0.2 kappa interval needs",
            corpus().len()
        );
    }

    /// Near-balanced, because κ's variance is worst when one class is rare —
    /// the paradox `2026-09-09-symbolic-evaluation-findings.md` §3 records.
    #[test]
    fn problems_and_clean_turns_are_near_balanced() {
        let problems = corpus().iter().filter(|c| c.gold.is_problem()).count();
        let share = problems as f32 / corpus().len() as f32;
        assert!(
            (0.35..=0.65).contains(&share),
            "problem share {share:.2} is outside the band that keeps kappa estimable"
        );
    }

    /// Both languages, in equal measure. An English-heavy corpus would report
    /// a friendlier number than this deployment will ever see.
    #[test]
    fn the_two_languages_are_balanced() {
        let cs = corpus().iter().filter(|c| c.lang == "cs").count();
        let en = corpus().iter().filter(|c| c.lang == "en").count();
        assert_eq!(cs + en, corpus().len(), "every case is cs or en");
        let diff = cs.abs_diff(en);
        assert!(diff <= 4, "cs={cs} en={en} — too lopsided to compare");
    }

    /// Both halves carry every kind, or the held-out number is about a
    /// different corpus than the one anything was chosen on.
    #[test]
    fn every_issue_kind_appears_in_both_splits() {
        for kind in [
            Issue::None,
            Issue::Reask,
            Issue::IgnoredQuestion,
            Issue::IgnoredRequest,
            Issue::Ungrounded,
            Issue::Correction,
        ] {
            for split in [Split::Dev, Split::Held] {
                let n = corpus()
                    .iter()
                    .filter(|c| c.gold == kind && c.split == split)
                    .count();
                assert!(
                    n >= 3,
                    "{} has only {n} case(s) in {split:?} — not enough to say anything",
                    kind.as_str()
                );
            }
        }
    }

    /// The two halves are close to the same size, so neither is a rump.
    #[test]
    fn the_splits_are_close_to_even() {
        let dev = corpus().iter().filter(|c| c.split == Split::Dev).count();
        let held = corpus().iter().filter(|c| c.split == Split::Held).count();
        assert!(dev.abs_diff(held) <= 6, "dev={dev} held={held}");
    }

    #[test]
    fn ids_are_unique_and_name_their_language_and_kind() {
        let mut seen = HashSet::new();
        for c in corpus() {
            assert!(seen.insert(c.id), "duplicate id {}", c.id);
            assert!(
                c.id.starts_with(c.lang),
                "{} does not start with its language",
                c.id
            );
        }
    }

    /// A clean turn has to be clean in every respect the corpus grades, or a
    /// scorer that gets it wrong is being blamed for the corpus's mistake.
    #[test]
    fn clean_cases_acted_when_the_tier_said_to() {
        for c in corpus().iter().filter(|c| c.gold == Issue::None) {
            if c.task_tier {
                assert!(
                    c.acted,
                    "{}: a clean task-tier turn that did nothing is an ignored request",
                    c.id
                );
            }
            assert!(
                !c.shown.is_empty(),
                "{}: nothing shown to be grounded in",
                c.id
            );
        }
    }

    /// The structural half of I6: every ignored-request case must be the shape
    /// the check reads, and no *other* case may be, or the label is ambiguous.
    #[test]
    fn only_ignored_request_cases_are_task_tier_and_idle() {
        for c in corpus() {
            let shape = c.task_tier && !c.acted;
            assert_eq!(
                shape,
                c.gold == Issue::IgnoredRequest,
                "{}: task_tier={} acted={} but gold={}",
                c.id,
                c.task_tier,
                c.acted,
                c.gold.as_str()
            );
        }
    }

    /// A re-ask and a correction live in the follow-up. A case labelled either
    /// with no follow-up would be unlabelable from what the evaluator sees.
    #[test]
    fn cases_whose_evidence_is_the_follow_up_have_one() {
        for c in corpus() {
            if matches!(c.gold, Issue::Reask | Issue::Correction) {
                assert!(c.next_user.is_some(), "{}: no follow-up to read", c.id);
            }
        }
    }

    /// Every ungrounded reply states something the material does not, and no
    /// clean reply does — checked with the engine's own interceptor, so the
    /// corpus cannot claim a grounding failure the shipping code disagrees
    /// with. Czech is exempt from the second half only where the check has no
    /// purchase (`extract_claims` keys on numbers, quotes and capitalised
    /// mid-sentence words), which is why the ungrounded Czech cases all state
    /// a number or a name.
    #[test]
    fn ungrounded_cases_are_ungrounded_by_the_shipping_check() {
        let mut wrong: Vec<String> = Vec::new();
        for c in corpus() {
            let material = nsengine::ground::Material::from_parts(c.shown);
            let spans = nsengine::ground::ungrounded(c.reply, &material);
            match c.gold {
                Issue::Ungrounded if spans.is_empty() => {
                    wrong.push(format!("{}: ungrounded, but nothing flagged", c.id))
                }
                Issue::None if !spans.is_empty() => {
                    wrong.push(format!("{}: clean, but flagged {spans:?}", c.id))
                }
                _ => {}
            }
        }
        assert!(
            wrong.is_empty(),
            "{}",
            wrong.join(
                "
"
            )
        );
    }
}
