use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vitral_core::VitralCore;
use vitral_core::clip::{ClipboardManager, ClipboardPayload};
use vitral_core::eng::{
    CandidateItem, EngineConfig, EngineState, InputAction, SchemaKind, SpatialAlt, SwipeDir,
    TouchKeyHint,
};
use vitral_core::imp::{ImportedLexiconIndex, LexiconFormat, compile_and_index};
use vitral_core::ret::{RetentionEngine, UserPhrase};

use SchemaKind::{DoublePinyinFlypy, RimeIce};

const SCHEMAS: [SchemaKind; 2] = [RimeIce, DoublePinyinFlypy];

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "vitral-candidate-quality-{}-{}",
            std::process::id(),
            RetentionEngine::now_epoch_ms()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!("Cannot remove {}: {error}", self.0.display());
        }
    }
}

struct Keyboard {
    engine: EngineState,
    clipboard: ClipboardManager,
    imported: ImportedLexiconIndex,
    retention: RetentionEngine,
}

impl Keyboard {
    fn new(assets: &Path) -> Self {
        Self::with_config(assets, EngineConfig::default())
    }

    fn with_config(assets: &Path, config: EngineConfig) -> Self {
        let mut engine = EngineState::new(EngineConfig {
            assets_dir: assets.display().to_string(),
            platform: "android".into(),
            pure_offline: true,
            ..config
        });
        // Both feature sets exercise the portable decoder with the same assets.
        engine.resolved_assets_dir = None;
        Self {
            engine,
            clipboard: ClipboardManager::with_device(8, "quality-test"),
            imported: ImportedLexiconIndex::new(Some(assets)),
            retention: RetentionEngine::default(),
        }
    }

    fn key(&mut self, key: u32) -> InputAction {
        self.engine
            .process_key(
                key,
                0,
                SwipeDir::None,
                &self.clipboard,
                &self.imported,
                &mut self.retention,
            )
            .unwrap()
    }

    fn touch(&mut self, key: u8, hint: TouchKeyHint) -> InputAction {
        self.engine
            .process_touch_key(
                u32::from(key),
                0,
                SwipeDir::None,
                hint,
                &self.clipboard,
                &self.imported,
                &mut self.retention,
            )
            .unwrap()
    }

    fn type_code(&mut self, code: &str) -> Vec<CandidateItem> {
        type_code(code, |key| self.key(key))
    }

    fn query(&mut self, schema: SchemaKind, code: &str) -> Vec<CandidateItem> {
        self.key(27);
        self.engine.set_schema(schema);
        self.type_code(code)
    }

    fn choose(&mut self, candidates: &[CandidateItem], word: &str) {
        let rank = rank(candidates, word, None);
        assert!(
            (1..=9).contains(&rank),
            "{word} unavailable: {:?}",
            preview(candidates, 9)
        );
        assert_eq!(
            self.key(u32::from(b'0') + rank as u32)
                .commit_text
                .as_deref(),
            Some(word)
        );
    }
}

fn type_code(code: &str, mut key: impl FnMut(u32) -> InputAction) -> Vec<CandidateItem> {
    code.bytes()
        .map(|byte| key(u32::from(byte)))
        .last()
        .unwrap()
        .candidates
}

fn rank(candidates: &[CandidateItem], word: &str, comment: Option<&str>) -> usize {
    candidates
        .iter()
        .position(|item| item.text == word && comment.is_none_or(|value| item.comment == value))
        .map_or(usize::MAX, |index| index + 1)
}

fn preview(candidates: &[CandidateItem], limit: usize) -> Vec<(&str, &str)> {
    candidates
        .iter()
        .take(limit)
        .map(|item| (item.text.as_str(), item.comment.as_str()))
        .collect()
}

#[derive(Default)]
struct Hits {
    total: usize,
    top1: usize,
    top5: usize,
}

impl Hits {
    fn record(&mut self, observed: usize) {
        self.total += 1;
        self.top1 += usize::from(observed == 1);
        self.top5 += usize::from(observed <= 5);
    }

    fn add(&mut self, other: &Self) {
        self.total += other.total;
        self.top1 += other.top1;
        self.top5 += other.top5;
    }

    fn check(&self, category: &str, top1: usize, top5: usize) {
        eprintln!(
            "{category}: Top-1 {}/{} ({:.2}%), Top-5 {}/{} ({:.2}%)",
            self.top1,
            self.total,
            percent(self.top1, self.total),
            self.top5,
            self.total,
            percent(self.top5, self.total)
        );
        assert!(
            self.top1 >= top1 && self.top5 >= top5,
            "{category} requires Top-1 {top1} and Top-5 {top5}"
        );
    }
}

fn percent(hits: usize, total: usize) -> f64 {
    100.0 * hits as f64 / total as f64
}

fn record(
    hits: &mut Hits,
    schema: SchemaKind,
    code: &str,
    word: &str,
    comment: Option<&str>,
    candidates: &[CandidateItem],
) {
    let observed = rank(candidates, word, comment);
    hits.record(observed);
    if observed != 1 {
        eprintln!(
            "{schema:?} {code} → {word} {comment:?} (rank {observed}): {:?}",
            preview(candidates, 5)
        );
    }
}

// Full-pinyin keystrokes, Xiaohe keystrokes, and the intended text.
const CHAT: &[(&str, &str, &str)] = &[
    ("wozhidaole", "wovidcle", "我知道了"),
    ("yihui'erjian", "yihverjm", "一会儿见"),
    ("womenmingtianjian", "womfmktmjm", "我们明天见"),
    ("nizainali", "nizdnali", "你在哪里"),
    ("nidaojialema", "nidcjxlema", "你到家了吗"),
    ("jintianxingqiji", "jbtmxkqiji", "今天星期几"),
    ("nishenmeshihouhuilai", "niufmeuihzhvld", "你什么时候回来"),
    ("mingtianzaishuo", "mktmzduo", "明天再说"),
    ("nichifanlema", "niiifjlema", "你吃饭了吗"),
    ("wochiguole", "woiigole", "我吃过了"),
    ("wozailushang", "wozdluuh", "我在路上"),
    ("wozhengzaikaihui", "wovgzdkdhv", "我正在开会"),
    ("woxianzaiyoudianmang", "woxmzdyzdmmh", "我现在有点忙"),
    ("dengyixiazaishuo", "dgyixxzduo", "等一下再说"),
    ("wogeinidadianhua", "wogwnidadmhx", "我给你打电话"),
    ("daojiageiwofaxiaoxi", "dcjxgwwofaxnxi", "到家给我发消息"),
    ("lushangxiaoxin", "luuhxnxb", "路上小心"),
    ("zaodianxiuxi", "zcdmxqxi", "早点休息"),
    ("jintianxinkule", "jbtmxbkule", "今天辛苦了"),
    ("zhoumoyukuai", "vzmoyukk", "周末愉快"),
    ("buyongkeqi", "buyskeqi", "不用客气"),
    ("buhaoyisi", "buhcyisi", "不好意思"),
    ("woyebuzhidao", "woyebuvidc", "我也不知道"),
    ("nishuodedui", "niuodedv", "你说得对"),
    ("wotongyinideyijian", "wotsyinideyijm", "我同意你的意见"),
    ("youshijianzailiao", "yzuijmzdln", "有时间再聊"),
    ("qingshaodengyixia", "qkucdgyixx", "请稍等一下"),
    ("zhegeduoshaoqian", "vegedoucqm", "这个多少钱"),
    ("mingtianjidianchufa", "mktmjidmiufa", "明天几点出发"),
    ("womenyiqiqu", "womfyiqiqu", "我们一起去"),
    ("wobangnikankan", "wobhnikjkj", "我帮你看看"),
    ("niganjuezenmeyang", "nigjjtzfmeyh", "你感觉怎么样"),
    ("wojintianhenkaixin", "wojbtmhfkdxb", "我今天很开心"),
    ("nixianhuiquba", "nixmhvquba", "你先回去吧"),
    ("womenxiacizaiyue", "womfxxcizdyt", "我们下次再约"),
    ("wozhengzaidengni", "wovgzddgni", "我正在等你"),
    ("woyijingdaole", "woyijkdcle", "我已经到了"),
    ("womashanghuilai", "womauhhvld", "我马上回来"),
    (
        "qingwenxishoujianzainali",
        "qkwfxiuzjmzdnali",
        "请问洗手间在哪里",
    ),
    ("nixianghedianshenme", "nixlhedmufme", "你想喝点什么"),
    ("meiguanxiwokeyideng", "mwgrxiwokeyidg", "没关系我可以等"),
    ("wowangledaiyaoshi", "wowhleddycui", "我忘了带钥匙"),
    ("jidedaisan", "jideddsj", "记得带伞"),
    ("wogeiniliuleweizhi", "wogwnilqlewwvi", "我给你留了位置"),
    ("qingbangwokaiyixiamen", "qkbhwokdyixxmf", "请帮我开一下门"),
    ("zaimangshenme", "zdmhufme", "在忙什么"),
    ("nixianzaiyoukongma", "nixmzdyzksma", "你现在有空吗"),
    ("womashangjiudao", "womauhjqdc", "我马上就到"),
    ("womenyihuierjian", "womfyihverjm", "我们一会儿见"),
    ("wogangdaojia", "woghdcjx", "我刚到家"),
    ("nixiangchishenme", "nixliiufme", "你想吃什么"),
    ("meiwentimingtianjian", "mwwftimktmjm", "没问题明天见"),
    ("jintianguodezenmeyang", "jbtmgodezfmeyh", "今天过得怎么样"),
];

const BOUNDARIES: &[(&str, &str, &str)] = &[
    ("chongqing", "isqk", "重庆"),
    ("zhongliang", "vsll", "重量"),
    ("zhongyao", "vsyc", "重要"),
    ("chongfu", "isfu", "重复"),
    ("zhangda", "vhda", "长大"),
    ("chengzhang", "igvh", "成长"),
    ("changdu", "ihdu", "长度"),
    ("kuaile", "kkle", "快乐"),
    ("yueqi", "ytqi", "乐器"),
    ("liaojie", "lnjp", "了解"),
    ("huanqian", "hrqm", "还钱"),
    ("haiyou", "hdyz", "还有"),
    ("zhuanji", "vrji", "传记"),
    ("chuanqi", "irqi", "传奇"),
    ("jihao", "jihc", "记号"),
    ("zhihou", "vihz", "之后"),
    ("zhiban", "vibj", "值班"),
    ("zhizhen", "vivf", "指针"),
    ("gandong", "gjds", "感动"),
    ("gandong", "gjds", "敢动"),
    ("licheng", "liig", "里程"),
    ("lishi", "liui", "历史"),
    ("shichang", "uiih", "市场"),
    ("shichang", "uiih", "时常"),
    ("huiyi", "hvyi", "会议"),
    ("huiyi", "hvyi", "回忆"),
    ("nv'er", "nver", "女儿"),
    ("yu'e", "yuee", "余额"),
    ("qi'e", "qiee", "企鹅"),
    ("hai'an", "hdan", "海岸"),
    ("fangan", "fhan", "方案"),
    ("xi'an", "xian", "西安"),
    ("shang'e", "uhee", "上颚"),
    ("chang'an", "ihan", "长安"),
    ("chongxin", "isxb", "重新"),
    ("yinhang", "ybhh", "银行"),
    ("hangye", "hhye", "行业"),
    ("yinyue", "ybyt", "音乐"),
    ("tadepengyou", "tadepgyz", "他的朋友"),
    ("manmandezou", "mjmjdezz", "慢慢地走"),
    ("zhecizuodehenhao", "vecizodehfhc", "这次做得很好"),
];

const TECHNICAL: &[(&str, &str, &str)] = &[
    ("shurufa", "uurufa", "输入法"),
    ("shujuku", "uujuku", "数据库"),
    ("caozuoxitong", "cczoxits", "操作系统"),
    ("rengongzhineng", "rfgsving", "人工智能"),
    ("jiqixuexi", "jiqixtxi", "机器学习"),
    ("shenjingwangluo", "ufjkwhlo", "神经网络"),
    ("shenduxuexi", "ufduxtxi", "深度学习"),
    ("yuandaima", "yrddma", "源代码"),
    ("banbenkongzhi", "bjbfksvi", "版本控制"),
    ("chixujicheng", "iixujiig", "持续集成"),
    ("daimashencha", "ddmaufia", "代码审查"),
    ("danyuanceshi", "djyrceui", "单元测试"),
    ("jichengceshi", "jiigceui", "集成测试"),
    ("huiguiceshi", "hvgvceui", "回归测试"),
    ("ceshiyongli", "ceuiysli", "测试用例"),
    ("neicunxielou", "nwcyxplz", "内存泄漏"),
    ("lajihuishou", "lajihvuz", "垃圾回收"),
    ("shujujiegou", "uujujpgz", "数据结构"),
    ("shijianfuzadu", "uijmfuzadu", "时间复杂度"),
    ("fuzaijunheng", "fuzdjyhg", "负载均衡"),
    ("fenbushixitong", "ffbuuixits", "分布式系统"),
    ("bingfakongzhi", "bkfaksvi", "并发控制"),
    ("xianchengchi", "xmigii", "线程池"),
    ("xiaoxiduilie", "xnxidvlp", "消息队列"),
    ("wangluoyanchi", "whloyjii", "网络延迟"),
    ("shujutongbu", "uujutsbu", "数据同步"),
    ("fangwenquanxian", "fhwfqrxm", "访问权限"),
    ("wenjianchuanshu", "wfjmiruu", "文件传输"),
    ("kehuduan", "kehudr", "客户端"),
    ("fuwuqi", "fuwuqi", "服务器"),
];

const MIXED: &[(SchemaKind, &str, &str, &str)] = &[
    (RimeIce, "xguang", "X光", "中英"),
    (DoublePinyinFlypy, "xgl", "X光", "中英"),
    (RimeIce, "txu", "T恤", "中英"),
    (DoublePinyinFlypy, "txu", "T恤", "中英"),
    (RimeIce, "upan", "U盘", "中英"),
    (DoublePinyinFlypy, "upj", "U盘", "中英"),
    (RimeIce, "api", "API", "英文"),
    (DoublePinyinFlypy, "api", "API", "英文"),
    (RimeIce, "gith", "GitHub", "英文"),
    (DoublePinyinFlypy, "gith", "GitHub", "英文"),
    (RimeIce, "pyth", "Python", "英文"),
    (DoublePinyinFlypy, "pyth", "Python", "英文"),
    (RimeIce, "upda", "update", "英文"),
    (DoublePinyinFlypy, "upda", "update", "英文"),
    (RimeIce, "rug", "如果", "ru guo"),
    (RimeIce, "rug", "rug", "英文"),
    (RimeIce, "can", "can", "英文"),
    (RimeIce, "you", "you", "英文"),
];

const CORRECTIONS: &[(SchemaKind, &str, &str, bool)] = &[
    (RimeIce, "nihoa", "你好", true),
    (RimeIce, "tna", "谈", true),
    (RimeIce, "nui", "牛", true),
    (DoublePinyinFlypy, "nhc", "你好", true),
    (DoublePinyinFlypy, "nixhc", "你好", true),
    (RimeIce, "diushi", "丢失", false),
    (RimeIce, "nigao", "你搞", false),
    (DoublePinyinFlypy, "dqui", "丢失", false),
    (DoublePinyinFlypy, "nigc", "你搞", false),
    (RimeIce, "xianzai", "现在", false),
    (DoublePinyinFlypy, "xmzd", "现在", false),
];

const POLLUTION: &[(&str, &str, &str)] = &[
    ("nihao", "nihc", "你好"),
    ("xianzai", "xmzd", "现在"),
    ("diushi", "dqui", "丢失"),
    ("nigao", "nigc", "你搞"),
    ("fangan", "fhan", "方案"),
    ("xiangmuguanli", "xlmugrli", "项目管理"),
    ("wogangdaojia", "woghdcjx", "我刚到家"),
    ("womashangjiudao", "womauhjqdc", "我马上就到"),
];

fn paired_queries(keyboard: &mut Keyboard, cases: &[(&str, &str, &str)]) -> Hits {
    let mut hits = Hits::default();
    for &(quanpin, xiaohe, word) in cases {
        for (schema, code) in SCHEMAS.into_iter().zip([quanpin, xiaohe]) {
            let candidates = keyboard.query(schema, code);
            record(&mut hits, schema, code, word, None, &candidates);
        }
    }
    hits
}

fn pollution(keyboard: &mut Keyboard, scratch: &Path) -> Hits {
    let mut baseline = Vec::new();
    for &(quanpin, xiaohe, word) in POLLUTION {
        for (schema, code) in SCHEMAS.into_iter().zip([quanpin, xiaohe]) {
            let observed = rank(&keyboard.query(schema, code), word, None);
            baseline.push((schema, code, word, observed));
        }
    }

    let now = RetentionEngine::now_epoch_ms();
    let old_ms = now - (keyboard.retention.half_life_sec * 1000.0 * 12.0) as u64;
    for _ in 0..64 {
        keyboard.retention.record_selection_at("拟好", old_ms, 1.0);
    }
    let ago = keyboard.retention.decayed_score_at("拟好", old_ms);
    assert!(ago > 1.0);
    assert!(keyboard.retention.decayed_score_at("拟好", now) < ago / 4095.0);

    for schema in SCHEMAS {
        let (greeting, arrival) = if schema == RimeIce {
            ("nihao", "wogangdaojia")
        } else {
            ("nihc", "woghdcjx")
        };
        for round in 0..12 {
            let candidates = keyboard.query(schema, greeting);
            if round % 4 == 3 {
                let mistake = candidates
                    .iter()
                    .take(9)
                    .find(|item| {
                        item.text != "你好"
                            && item.text.chars().count() == 2
                            && item.comment != "纠错"
                    })
                    .unwrap()
                    .text
                    .clone();
                keyboard.choose(&candidates, &mistake);
                let mistaken_score = keyboard.retention.decayed_score(&mistake);
                let undo = keyboard.key(8);
                assert_eq!(undo.undo_commit_text.as_deref(), Some(mistake.as_str()));
                keyboard.choose(&undo.candidates, "你好");
                assert!(keyboard.retention.decayed_score(&mistake) < mistaken_score);
            } else {
                keyboard.choose(&candidates, "你好");
            }
            let candidates = keyboard.type_code(arrival);
            keyboard.choose(&candidates, "我刚到家");
        }
    }

    let hotwords = scratch.join("high-weight.tsv");
    fs::write(
        &hotwords,
        "拟号机\tni hao ji\t1000000\n港到家\tgang dao jia\t1000000\n蓝枢引擎\tlan shu yin qing\t1000000\n",
    )
    .unwrap();
    let report =
        compile_and_index(&mut keyboard.imported, &hotwords, LexiconFormat::Tsv, None).unwrap();
    assert_eq!(report.imported_count, 3);
    for schema in SCHEMAS {
        let code = if schema == RimeIce {
            "lanshuyinqing"
        } else {
            "ljuuybqk"
        };
        assert!(rank(&keyboard.query(schema, code), "蓝枢引擎", None) <= 5);
    }

    let mut hits = Hits::default();
    for (schema, code, word, before) in baseline {
        let candidates = keyboard.query(schema, code);
        let after = rank(&candidates, word, None);
        assert!(
            after <= before,
            "{schema:?} {code} {word}: rank {before} → {after}"
        );
        record(&mut hits, schema, code, word, None, &candidates);
    }
    hits
}

fn required_assets() -> PathBuf {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets");
    for file in [
        "cn_dicts/8105.dict.yaml",
        "cn_dicts/41448.dict.yaml",
        "cn_dicts/base.dict.yaml",
        "cn_dicts/ext.dict.yaml",
        "cn_dicts/others.dict.yaml",
        "en_dicts/en.dict.yaml",
        "en_dicts/en_ext.dict.yaml",
        "en_dicts/cn_en.txt",
        "en_dicts/cn_en_flypy.txt",
    ] {
        let path = assets.join("rime").join(file);
        assert!(
            path.is_file(),
            "Missing Rime dictionary {}. Run ./scripts/fetch-assets.sh first.",
            path.display()
        );
    }
    assets
}

#[test]
fn copying_text_keeps_typing_candidates_unchanged() {
    let assets = required_assets();
    let scratch = Scratch::new();
    let config = EngineConfig {
        assets_dir: assets.display().to_string(),
        data_dir: scratch.0.display().to_string(),
        pure_offline: true,
        // Select the portable decoder for both schemas.
        syllable_split: true,
        fuzzy_pinyin: true,
        ..EngineConfig::default()
    };
    let core = VitralCore::init(&serde_json::to_string(&config).unwrap()).unwrap();
    let candidates = |schema, code| {
        core.process_key(27, 0, SwipeDir::None).unwrap();
        core.set_schema(schema);
        type_code(code, |key| core.process_key(key, 0, SwipeDir::None).unwrap())
            .into_iter()
            .map(|item| (item.text, item.comment))
            .collect::<Vec<_>>()
    };
    let baseline = [candidates(RimeIce, "yunli"), candidates(DoublePinyinFlypy, "yyli")];

    let content = "今天讨论了云璃输入法架构。OpenAI";
    let state = core.clipboard_push(ClipboardPayload {
        id: None,
        content: content.into(),
        mime_type: "text/plain".into(),
        source_device: "quality-test".into(),
        timestamp_ms: 0,
    });
    assert_eq!(state.ring[0].content, content);
    assert_eq!(core.personalization_stats().phrase_count, 0);
    assert_eq!(core.personalization_stats().english_count, 0);
    assert_eq!(
        [candidates(RimeIce, "yunli"), candidates(DoublePinyinFlypy, "yyli")],
        baseline
    );
}

#[test]
fn offline_candidate_quality() {
    let assets = required_assets();
    let scratch = Scratch::new();
    let mut keyboard = Keyboard::with_config(&assets, EngineConfig {
        undo_commit_on_backspace: true,
        ..EngineConfig::default()
    });
    let chat = paired_queries(&mut keyboard, CHAT);
    let boundaries = paired_queries(&mut keyboard, BOUNDARIES);
    let technical = paired_queries(&mut keyboard, TECHNICAL);

    let mut mixed = Hits::default();
    for &(schema, code, word, comment) in MIXED {
        let candidates = keyboard.query(schema, code);
        if word == code && matches!(code, "rug" | "can" | "you") {
            assert!((3..=5).contains(&rank(&candidates, word, Some(comment))));
            assert!(candidates[..2].iter().all(|item| !item.text.is_ascii()));
        }
        record(&mut mixed, schema, code, word, Some(comment), &candidates);
    }

    let mut corrections = Hits::default();
    for &(schema, code, word, corrected) in CORRECTIONS {
        let candidates = keyboard.query(schema, code);
        record(
            &mut corrections,
            schema,
            code,
            word,
            corrected.then_some("纠错"),
            &candidates,
        );
        if !corrected {
            assert_eq!(
                candidates[0].text,
                word,
                "{code}: {:?}",
                preview(&candidates, 5)
            );
            assert!(
                candidates.iter().all(|item| item.comment != "纠错"),
                "{code}: {:?}",
                preview(&candidates, candidates.len())
            );
        }
    }
    let pollution = pollution(&mut keyboard, &scratch.0);
    let mut total = Hits::default();
    for (name, hits, top1, top5) in [
        ("long sentences and chat", chat, 90, 98),
        (
            "syllable boundaries, homophones and context",
            boundaries,
            75,
            97,
        ),
        ("technical vocabulary", technical, 90, 98),
        ("mixed input and English downranking", mixed, 83, 100),
        ("keyboard typos and exact matches", corrections, 90, 100),
        (
            "ranking after accidental selections and imports",
            pollution,
            100,
            100,
        ),
    ] {
        hits.check(
            name,
            (hits.total * top1).div_ceil(100),
            (hits.total * top5).div_ceil(100),
        );
        total.add(&hits);
    }
    assert!(total.total >= 200);
    total.check(
        "all candidate quality cases",
        (total.total * 90).div_ceil(100),
        (total.total * 98).div_ceil(100),
    );
}

// Model a staggered QWERTY layout in quarter-key units.
fn qwerty_position(key: u8) -> (i32, i32) {
    for (row, keys, offset) in [
        (0, b"qwertyuiop".as_slice(), 0),
        (4, b"asdfghjkl".as_slice(), 1),
        (8, b"zxcvbnm".as_slice(), 3),
    ] {
        if let Some(col) = keys.iter().position(|&ch| ch == key) {
            return (4 * col as i32 + offset, row);
        }
    }
    panic!("Not a QWERTY letter: {}", char::from(key));
}

fn touch_query(
    keyboard: &mut Keyboard,
    schema: SchemaKind,
    typed: &str,
    alternative: &str,
) -> (usize, Vec<CandidateItem>) {
    assert_eq!(typed.len(), alternative.len());
    keyboard.engine.set_schema(schema);
    let mut substitutions = 0;
    let candidates = typed
        .bytes()
        .zip(alternative.bytes())
        .map(|(primary, ch)| {
            let mut hint = TouchKeyHint::default();
            if primary != ch {
                let (x1, y1) = qwerty_position(primary);
                let (x2, y2) = qwerty_position(ch);
                let distance = (x1 - x2).pow(2) + (y1 - y2).pow(2);
                assert!(distance <= 25, "{typed} → {alternative}: nonadjacent keys");
                hint = TouchKeyHint::new(primary, [Some(SpatialAlt { ch, penalty: 1.0 }), None]);
                assert!(!hint.is_empty());
                substitutions += 1;
            }
            keyboard.touch(primary, hint)
        })
        .last()
        .unwrap()
        .candidates;
    (substitutions, candidates)
}

#[test]
fn mobile_qwerty_spatial_recovery_and_exact_word_guard() {
    let assets = required_assets();
    let mut keyboard = Keyboard::with_config(
        &assets,
        EngineConfig {
            typo_correction_level: "conservative".into(),
            ..EngineConfig::default()
        },
    );
    let mut recovery = Hits::default();
    let mut baseline = Hits::default();
    for (schema, cases) in [
        (
            RimeIce,
            [
                ("nihao", "nihso", "njhso", "你好"),
                ("nihao", "bihao", "bihso", "你好"),
                ("nihao", "nihzo", "njhzo", "你好"),
                ("xianzai", "xjanzai", "xjanzsi", "现在"),
                ("wogangdaojia", "wogangdaojja", "wogangdsojja", "我刚到家"),
                (
                    "womashangjiudao",
                    "womsshangjiudao",
                    "womsshangjiudap",
                    "我马上就到",
                ),
            ],
        ),
        (
            DoublePinyinFlypy,
            [
                ("woxliipkgo", "woxkiipkgo", "woxkiipkgp", "我想吃苹果"),
                ("nihc", "bihc", "bjhc", "你好"),
                ("woghdcjx", "qoghdcjx", "qoghdcjz", "我刚到家"),
                ("nixliiufme", "nixkiiufme", "nixkiiugme", "你想吃什么"),
                ("xmzd", "xmzs", "zmzs", "现在"),
                ("woghdcjx", "woghdcjz", "woghfcjz", "我刚到家"),
            ],
        ),
    ] {
        for (misses, label) in [(1, "one key"), (2, "two keys")] {
            let mut hits = Hits::default();
            let mut without_hints = Hits::default();
            for (intended, one, two, word) in cases {
                let typed = if misses == 1 { one } else { two };
                without_hints.record(rank(&keyboard.query(schema, typed), word, None));
                let (substitutions, candidates) =
                    touch_query(&mut keyboard, schema, typed, intended);
                assert_eq!(substitutions, misses, "{typed} → {intended}");
                record(&mut hits, schema, typed, word, None, &candidates);
            }
            eprintln!(
                "{schema:?} {label}: Top-1 {}/{}, Top-5 {}/{} (without hints: Top-1 {}, Top-5 {})",
                hits.top1,
                hits.total,
                hits.top5,
                hits.total,
                without_hints.top1,
                without_hints.top5
            );
            recovery.add(&hits);
            baseline.add(&without_hints);
        }
    }
    let mut false_corrections = 0;
    let guards = [
        (RimeIce, "diushi", "xiushi", "丢失"),
        (RimeIce, "nigao", "nihao", "你搞"),
        (DoublePinyinFlypy, "dqui", "xqui", "丢失"),
        (DoublePinyinFlypy, "nigc", "nihc", "你搞"),
    ];
    for (schema, typed, alternative, word) in guards {
        assert_eq!(rank(&keyboard.query(schema, typed), word, None), 1);
        let (substitutions, candidates) = touch_query(&mut keyboard, schema, typed, alternative);
        assert_eq!(substitutions, 1);
        if rank(&candidates, word, None) != 1 {
            false_corrections += 1;
            eprintln!(
                "{schema:?} {typed} protection failed: {:?}",
                preview(&candidates, 5)
            );
        }
    }
    eprintln!(
        "2D QWERTY: Top-5 recovery {}/{} ({:.2}%), false corrections {false_corrections}/{} ({:.2}%)",
        recovery.top5,
        recovery.total,
        percent(recovery.top5, recovery.total),
        guards.len(),
        percent(false_corrections, guards.len())
    );
    assert!(recovery.top5 * 100 >= recovery.total * 90);
    assert!(
        recovery.top5 > baseline.top5,
        "no recovery gain from touch hints"
    );
    assert_eq!(false_corrections, 0);
}

#[test]
fn precision_selection_round_trips_in_both_schemas() {
    let assets = required_assets();
    assert!(assets.join("handwriting/cjkvi_ids.txt").is_file());
    for (schema, doctor, guest, bright, tail) in [
        (RimeIce, "yisheng", "haoke", "ming", "sheng"),
        (DoublePinyinFlypy, "yiug", "hcke", "mk", "ug"),
    ] {
        let mut keyboard = Keyboard::new(&assets);
        for (key, expected, reading) in [(b'[', "医", "yi"), (b']', "生", tail)] {
            assert_eq!(rank(&keyboard.query(schema, doctor), "医生", None), 1);
            let action = keyboard.key(u32::from(key));
            assert_eq!(action.commit_text.as_deref(), Some(expected));
            assert!(action.preedit.text.is_empty());
            assert!(keyboard.retention.code_preferences[reading].contains_key(expected));
        }
        assert!(!keyboard.retention.code_preferences.contains_key(doctor));

        let tone_cases = [
            ('1', "妈"),
            ('2', "麻"),
            ('3', "马"),
            ('4', "骂"),
            ('5', "吗"),
        ];
        for (tone, expected) in tone_cases {
            let code = format!("ma`{tone}");
            let candidates = keyboard.query(schema, &code);
            assert_eq!(keyboard.engine.raw_input(), code);
            assert!(
                rank(&candidates, expected, None) <= 9,
                "{schema:?} {code}: {:?}",
                preview(&candidates, 9)
            );
            if tone == '1' {
                assert_eq!(rank(&candidates, "马", None), usize::MAX);
            } else if tone == '3' {
                assert_eq!(rank(&candidates, "妈", None), usize::MAX);
            }
            keyboard.choose(&candidates, expected);
        }
        let candidates = keyboard.query(schema, &format!("{guest}`4"));
        assert!(
            rank(&candidates, "好客", None) <= 9,
            "{schema:?}: {:?}",
            preview(&candidates, 9)
        );
        keyboard.choose(&candidates, "好客");
        let mut candidates = keyboard.query(schema, &format!("{guest}`3"));
        loop {
            assert_eq!(rank(&candidates, "好客", None), usize::MAX, "{schema:?}");
            if !keyboard.engine.current_page.has_next {
                break;
            }
            candidates = keyboard.key(0xff56).candidates;
        }

        for (base, suffix, expected) in
            [("de", "b", "的"), ("de", "tu", "地"), (bright, "ry", "明")]
        {
            keyboard.engine.resolved_assets_dir = None;
            let unfiltered = keyboard.query(schema, base);
            let page = keyboard.engine.current_page.clone();
            let before = keyboard.retention.export_crdt_delta("precision").seq;
            // The IDS index reads its bundled data from resolved_assets_dir.
            keyboard.engine.resolved_assets_dir = Some(assets.clone());
            let filtered = keyboard.type_code(&format!("`{suffix}"));
            assert_eq!(
                rank(&filtered, expected, None),
                1,
                "{schema:?} {base}`{suffix}"
            );
            assert_eq!(keyboard.engine.raw_input(), format!("{base}`{suffix}"));
            let restored = (0..=suffix.len()).map(|_| keyboard.key(8)).last().unwrap();
            assert_eq!(keyboard.engine.raw_input(), base);
            assert_eq!(restored.page.has_next, page.has_next);
            assert_eq!(
                preview(&restored.candidates, usize::MAX),
                preview(&unfiltered, usize::MAX)
            );
            assert_eq!(
                keyboard.retention.export_crdt_delta("precision").seq,
                before
            );
        }
        eprintln!(
            "{schema:?} precision: 2 word edges, 5 tones, 2 phrase-tone cases, 3 reversible component filters"
        );
    }
}

#[test]
fn retention_remains_stable_through_1000_input_rounds() {
    const ROUNDS: usize = 1000;
    const CAPACITY: usize = 4096;
    let assets = required_assets();
    let mut keyboard = Keyboard::with_config(&assets, EngineConfig {
        undo_commit_on_backspace: true,
        ..EngineConfig::default()
    });
    let mut mistakes = 0;
    let mut recovered = 0;
    let mut checked_after_mistake = 0;
    for (schema, greeting, arrival) in [
        (RimeIce, "nihao", "wogangdaojia"),
        (DoublePinyinFlypy, "nihc", "woghdcjx"),
    ] {
        keyboard.engine.set_schema(schema);
        for round in 0..ROUNDS / SCHEMAS.len() {
            let candidates = keyboard.type_code(greeting);
            assert_eq!(
                rank(&candidates, "你好", None),
                1,
                "{schema:?} round {round}"
            );
            let candidates = if round % 25 == 24 {
                let mistake = candidates
                    .iter()
                    .take(9)
                    .find(|item| {
                        item.text != "你好"
                            && item.text.chars().count() == 2
                            && item.comment != "纠错"
                    })
                    .expect("an accidental two-character selection must be available")
                    .text
                    .clone();
                keyboard.choose(&candidates, &mistake);
                mistakes += 1;
                assert!(
                    keyboard.retention.decayed_score(&mistake)
                        < keyboard.retention.decayed_score("你好"),
                    "{schema:?} round {round}: {mistake} outranked repeated use by frecency"
                );
                let next = if round % 50 == 24 {
                    let mistaken_score = keyboard.retention.decayed_score(&mistake);
                    let undo = keyboard.key(8);
                    assert_eq!(undo.undo_commit_text.as_deref(), Some(mistake.as_str()));
                    assert_eq!(keyboard.engine.raw_input(), greeting);
                    keyboard.choose(&undo.candidates, "你好");
                    assert!(keyboard.retention.decayed_score(&mistake) < mistaken_score);
                    recovered += 1;
                    keyboard.type_code(greeting)
                } else {
                    checked_after_mistake += 1;
                    keyboard.type_code(greeting)
                };
                assert_eq!(
                    rank(&next, "你好", None),
                    1,
                    "{schema:?} round {round} after {mistake}"
                );
                next
            } else {
                candidates
            };
            keyboard.choose(&candidates, "你好");
            if round % 5 == 0 {
                let candidates = keyboard.type_code(arrival);
                assert_eq!(
                    rank(&candidates, "我刚到家", None),
                    1,
                    "{schema:?} round {round}"
                );
                keyboard.choose(&candidates, "我刚到家");
            }
        }
    }
    assert_eq!(mistakes, 40);
    assert_eq!(recovered, 20);
    assert_eq!(checked_after_mistake, 20);

    let now = RetentionEngine::now_epoch_ms();
    let after_12_half_lives = now + (keyboard.retention.half_life_sec * 12_000.0).round() as u64;
    let before_decay = keyboard.retention.decayed_score_at("你好", now);
    assert!(before_decay > 1.0);
    let after_decay = keyboard
        .retention
        .decayed_score_at("你好", after_12_half_lives);
    assert!((after_decay / before_decay * 4096.0 - 1.0).abs() < 1e-6);
    keyboard
        .retention
        .record_selection_at("你好", after_12_half_lives, 1.0);
    assert!(
        keyboard
            .retention
            .decayed_score_at("你好", after_12_half_lives)
            > 1.0
    );

    // Seed two device snapshots below capacity and let the real merge evict old entries.
    let mut local = keyboard.retention.export_crdt_delta("local");
    let mut remote = RetentionEngine::default().export_crdt_delta("remote");
    for i in 0..CAPACITY + 64 {
        let word = format!("词{}", char::from_u32(0x4e00 + i as u32).unwrap());
        let (count, updated_ms) = if i < 64 {
            (128, now)
        } else {
            (1, after_12_half_lives)
        };
        let delta = if i % 2 == 0 { &mut local } else { &mut remote };
        delta.user_phrases.insert(
            word.clone(),
            UserPhrase {
                word,
                xiaohe: "ciyv".into(),
                quanpin: "ci yu".into(),
                acronym: "cy".into(),
                count,
                updated_ms,
                source: "typed".into(),
            },
        );
    }
    assert!(local.user_phrases.len() < CAPACITY && remote.user_phrases.len() < CAPACITY);
    let mut replicas = [RetentionEngine::default(), RetentionEngine::default()];
    for (replica, order) in replicas
        .iter_mut()
        .zip([[&local, &remote], [&remote, &local]])
    {
        for delta in order {
            replica.merge_crdt_delta(delta);
        }
        assert_eq!(replica.stats().phrase_count, CAPACITY);
    }
    let expected = replicas[0].export_crdt_delta("check");
    assert!(
        expected
            .user_phrases
            .values()
            .all(|phrase| phrase.updated_ms == after_12_half_lives)
    );
    for replica in &mut replicas {
        for deltas in [&[][..], &[&local, &remote][..]] {
            for delta in deltas {
                replica.merge_crdt_delta(delta);
            }
            let actual = replica.export_crdt_delta("check");
            assert_eq!(actual.user_phrases, expected.user_phrases);
            assert_eq!(actual.frecency, expected.frecency);
            assert_eq!(actual.code_preferences, expected.code_preferences);
        }
    }
    eprintln!(
        "Retention: {ROUNDS} rounds, {mistakes} misselections, {recovered} one-round undo recoveries, {checked_after_mistake} stable after uncorrected misselections, decay {:.6}× after 12 half-lives, {CAPACITY} phrases on both replicas after merge and replay",
        after_decay / before_decay
    );
}

fn report_latency(label: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let percentile = |p: usize| samples[(samples.len() * p).div_ceil(100) - 1];
    eprintln!(
        "{label}: {} warmed final-keystroke samples, P50 {:.3} ms, P95 {:.3} ms, P99 {:.3} ms",
        samples.len(),
        percentile(50).as_secs_f64() * 1000.0,
        percentile(95).as_secs_f64() * 1000.0,
        percentile(99).as_secs_f64() * 1000.0,
    );
    #[cfg(not(debug_assertions))]
    assert!(
        percentile(99) < Duration::from_millis(250),
        "{label}: release P99 exceeds 250 ms"
    );
}

#[test]
fn warmed_candidate_query_latency_percentiles() {
    const ROUNDS: usize = 25;
    let assets = required_assets();
    let mut keyboard = Keyboard::new(&assets);
    let cases = [
        (
            "short",
            [
                ("nihao", "nihc", "你好"),
                ("yinhang", "ybhh", "银行"),
                ("shujuku", "uujuku", "数据库"),
            ],
        ),
        (
            "long",
            [
                ("nixianzaiyoukongma", "nixmzdyzksma", "你现在有空吗"),
                ("womashangjiudao", "womauhjqdc", "我马上就到"),
                ("jintianguodezenmeyang", "jbtmgodezfmeyh", "今天过得怎么样"),
            ],
        ),
    ];
    for (schema, index) in SCHEMAS.into_iter().zip(0..) {
        for (length, cases) in cases {
            let cases = cases.map(|(quanpin, xiaohe, word)| ([quanpin, xiaohe][index], word));
            for (code, word) in cases {
                assert_eq!(rank(&keyboard.query(schema, code), word, None), 1);
            }
            let mut samples = Vec::with_capacity(ROUNDS * cases.len());
            for _ in 0..ROUNDS {
                for (code, word) in cases {
                    keyboard.engine.set_schema(schema);
                    let (last, prefix) = code.as_bytes().split_last().unwrap();
                    for &key in prefix {
                        keyboard.key(u32::from(key));
                    }
                    let start = Instant::now();
                    let action = keyboard.key(u32::from(*last));
                    samples.push(start.elapsed());
                    assert_eq!(rank(&action.candidates, word, None), 1);
                }
            }
            report_latency(&format!("{schema:?} {length}"), &mut samples);
        }
    }
}
