use cold::{link::link, opt::parse_opts};
use tracing::info;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // 将 arg[0] 跳过后，获取从 arg[1] 开始的所有命令行参数，赋值给 args
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    info!("Launched with args: {:?}", args);

    // parse arguments
    let opt = parse_opts(&args)?;

    // 打印 struct Opt
    info!("Parsed options: {opt:?}");

    link(&opt)?;
    Ok(())
}
