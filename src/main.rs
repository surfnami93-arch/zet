// 外部ライブラリ
use num2words::Num2Words;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

// 標準ライブラリ
use std::fs;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

fn input(value: &str) -> String {
    let mut input = String::new();

    print!("{}", value);
    io::stdout().flush().unwrap(); // ← これを追加

    io::stdin().read_line(&mut input).expect("input error");

    input.trim().to_string()
}

// 変更点:
// 以前の open_new_console() (FreeConsole → AllocConsole で新規ウィンドウを
// 割り当てる処理) を削除。呼び出しもなくしたので、print! は
// cargo run を実行した「今の」コンソールにそのまま出力され続ける。
// 新規ウィンドウを作らない = 既存のwindowを使い回す、という変更。

/// 起動直後などで応答がまだ来ていない段階を「静か」と誤判定しないための最低待機時間
const MIN_WAIT: Duration = Duration::from_secs(1);
/// 出力がこの時間止まったら「処理が落ち着いた」とみなす
const QUIET_PERIOD: Duration = Duration::from_millis(800);
/// 出力が続いていても、これ以上は待たない上限(無限待ちの保険)
const MAX_WAIT: Duration = Duration::from_secs(30);

struct Session {
    writer: Box<dyn Write + Send>,
    /// 最後に出力を受信した時刻。読み取りスレッドが更新し、command_sendが監視する。
    last_activity: Arc<Mutex<Instant>>,
}

static SESSION: OnceLock<Mutex<Session>> = OnceLock::new();

/// 初回だけPowerShellを起動し、以降はそのセッションを使い回す。
fn get_session() -> &'static Mutex<Session> {
    SESSION.get_or_init(|| {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("Failed to create PTY.");

        let mut child = pair
            .slave
            .spawn_command(CommandBuilder::new("powershell.exe"))
            .expect("Failed to launch PowerShell.");
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .expect("Failed to retrieve the writer.");
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("Failed to retrieve the reader.");
        // masterはSendではないためstaticに保持できない。dropするとPTYごと閉じるので、
        // Dropを走らせずそのまま生かしておく。
        std::mem::forget(pair.master);

        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let last_activity_reader = Arc::clone(&last_activity);

        thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // 既存のコンソール(今のプロセスの標準出力)にそのまま出す
                        print!("{}", String::from_utf8_lossy(&buf[..n]));
                        let _ = std::io::stdout().flush();
                        *last_activity_reader.lock().unwrap() = Instant::now();
                    }
                }
            }
        });
        thread::spawn(move || {
            let _ = child.wait();
            // PowerShell(子プロセス)がexitなどで終了したら、それを検知できるのは
            // このスレッドだけ。メインスレッドはstdin.read_line()でブロックして
            // いるだけなので、何もしないと気づかず待ち続けてしまう
            // (Ctrl+Cで強制終了するしかなくなっていた原因)。
            // child.wait()が返った = PowerShellが終わった、ということなので、
            // ここでプロセス全体を終了させ、read_lineのブロックごと抜ける。
            //
            // ただし、中の(ssh先などの)セッションが文字色を変えるANSIエスケープ
            // (例: 緑にする \x1b[32m)を出したまま途中で出力が止まっていると、
            // その色指定がリセットされずに残ってしまい、終了後の外側のプロンプト
            // まで色がついて見える。exitする前に \x1b[0m (全属性リセット)を
            // 書き出しておくことで、この色の引き継ぎを防ぐ。
            print!("\x1b[0m");
            let _ = std::io::stdout().flush();
            std::process::exit(0);
        });

        Mutex::new(Session {
            writer,
            last_activity,
        })
    })
}

/// 人間の入力欄から渡された文字列だけを引数に取り、起動済みのPowerShellセッションに送り込む。
/// 出力がQUIET_PERIOD止まるまで待ってから戻るので、時間のかかるコマンドでも
/// 応答が続いている間はブロックし続ける。
///
/// 書き込みから完了待ちまでの間、セッションのロックを保持しっぱなしにしている。
/// これにより、この呼び出しが終わる前に別スレッドから(たとえば人間の手入力から)
/// command_sendが呼ばれても、ロック待ちで自動的にブロックされ、
/// PTYへの書き込みが途中で割り込むことはなくなる。
fn command_send(input: &str) {
    let mut session = get_session().lock().unwrap();

    session.writer.write_all(input.as_bytes()).unwrap();
    // EnterはCR(\r)のみでよい。\r\nにすると、CRが改行に変換された後に
    // 残った\nがもう一度「空コマンドのEnter」として処理され、
    // 空行のプロンプトがもう一つ余分に出てしまう。
    session.writer.write_all(b"\r").unwrap();
    session.writer.flush().unwrap();
    *session.last_activity.lock().unwrap() = Instant::now();

    let start = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(50));
        let elapsed = start.elapsed();
        // 最低待機時間に達するまでは、まだ応答が来ていなくても完了とみなさない
        if elapsed < MIN_WAIT {
            continue;
        }
        let quiet_for = session.last_activity.lock().unwrap().elapsed();
        if quiet_for >= QUIET_PERIOD || elapsed >= MAX_WAIT {
            break;
        }
    }
    // session はここでスコープを抜けて解放される
}

/// 自動入力(command_send)が現在実行中かどうかを判定する。
/// GUIの入力欄をその間だけ無効化する、といった用途に使う。
/// (command_sendのようにブロックはせず、すぐ結果を返す)
#[allow(dead_code)]
fn is_busy() -> bool {
    get_session().try_lock().is_err()
}

fn main() {
    // open_new_console() の呼び出しを削除。
    // これにより新しいコンソールウィンドウは作られず、
    // cargo run を実行した既存のウィンドウがそのまま使われる。

    let commands = setup();

    // 変更点: 以前は commands.into_iter().map(...) を let _ = で受けていたが、
    // map はイテレータアダプタで遅延評価のため、for_each や collect などで
    // 消費しない限り中のクロージャ(command_send)は一度も実行されない。
    // これが「コマンドが送信されない」原因だったので、for ループに変更して
    // 確実に1つずつ command_send を呼ぶようにした。
    for cmd in &commands {
        command_send(cmd);
    }

    // 自動入力が終わったあと、main()がそのまま return するとプロセスごと
    // 終了してしまい、せっかく開いたssh接続も一緒に閉じてしまう。
    // それを防ぐため、ここでユーザーのキー入力を待つループに入る。
    // ユーザーが何も操作しなければこの read_line でブロックし続けるので、
    // 自動では終了しなくなる。入力された行はそのままセッション(PTY)に
    // command_send で送り込まれるので、以降は手入力でssh先を操作できる。
    //
    // 終了させたい場合は、Ctrl+D (EOF) を送るか、Ctrl+Cでプロセスを
    // 強制終了する。
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // EOF (Ctrl+D など) が来たら終了
            Ok(_) => {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                command_send(trimmed);
            }
            Err(_) => break,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct FsState {
    commands: Vec<String>,
    variables: std::collections::HashMap<String, String>,
}

fn save_state(state: &FsState) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(state).unwrap();
    fs::write("state.json", json)
}
fn load_state() -> FsState {
    match fs::read_to_string("state.json") {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| FsState {
            commands: Vec::new(),
            variables: std::collections::HashMap::new(),
        }),
        Err(_) => FsState {
            commands: Vec::new(),
            variables: std::collections::HashMap::new(),
        },
    }
}

fn setup() -> Vec<String> {
    let mut state = load_state();

    if !state.variables.contains_key("legacy_start") {
        let mut count = 0;
        let mut content: Vec<String> = Vec::new();

        println!("\nwelcome zet!\n -------- Initial settings --------");

        loop {
            count += 1;

            let n = Num2Words::new(count).ordinal().to_words().unwrap();

            let answer = input(&format!(
                "Please enter the {}st command to run automatically (or 'a' to finish):",
                n
            ));

            if answer == "a" {
                break;
            }

            if answer != "" {
                content.push(answer);
            }
        }

        state
            .variables
            .insert("legacy_start".to_string(), "true".to_string());

        state.commands = content.clone();
        let _ = save_state(&state);

        println!("sucess!");
        println!(
            "You can change the initial settings in the `state.json` file located in the same folder."
        );

        content
    } else {
        state.commands
    }
}
