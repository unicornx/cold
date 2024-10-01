use anyhow::{anyhow, bail};

// 参考 https://maskray.me/blog/2020-11-15-explain-gnu-linker-options
// -Bstatic, --whole-archive, --as-needed等都是表示boolean狀態的position-dependent選項。
// --push-state可以保存這些選項的boolean狀態，--pop-state則會還原。
/// handle --push-state/--pop-state
#[derive(Debug, Copy, Clone)]
struct OptStack {
    // 参考 https://blog.csdn.net/itworld123/article/details/124467173
    // 在生成可执行文件的时候，通过 -lxxx 选项指定需要链接的库文件。以动态库为例，
    // 如果我们指定了一个需要链接的库，则连接器会在可执行文件的文件头中会记录下
    // 该库的信息。而后，在可执行文件运行的时候，动态加载器会读取文件头信息，并
    // 加载所有的链接库。在这个过程中，如果用户指定链接了一个毫不相关的库，则这
    // 个库在最终的可执行程序运行时也会被加载，如果类似这样的不相关库很多，会明
    // 显拖慢程序启动过程。
    // 这时，通过指定 --as-needed 选项，链接过程中，链接只会检查所有的依赖库，
    // 没有实际被引用的库，不再写入可执行文件头。最终生成的可执行文件头中包含的
    // 都是必要的链接库信息。
    // --no-as-needed 选项不会做这样的检查，会把用户指定的链接库完全写入可执行文件中。
    /// --as-needed
    pub as_needed: bool,
    /// -static
    pub link_static: bool,
}

#[derive(Debug, Clone)]
pub struct FileOpt {
    pub name: String,
    /// --as-needed
    pub as_needed: bool,
}

#[derive(Debug, Clone)]
pub struct LibraryOpt {
    pub name: String,
    /// --as-needed
    pub as_needed: bool,
    /// -static
    pub link_static: bool,
}

#[derive(Debug, Clone)]
pub enum ObjectFileOpt {
    /// ObjectFile
    File(FileOpt),
    /// -l namespec
    Library(LibraryOpt),
    /// --start-group
    StartGroup,
    /// --end-group
    EndGroup,
}

#[derive(Debug, Clone)]
pub struct HashStyle {
    pub sysv: bool,
    pub gnu: bool,
}

impl Default for HashStyle {
    fn default() -> Self {
        Self {
            sysv: true,
            gnu: true,
        }
    }
}

// 有关 derive 的介绍：https://doc.rust-lang.org/rust-by-example/trait/derive.html
// Default, to create an empty instance of a data type. 所以我们就可以调用 Opt::default();
#[derive(Debug, Clone, Default)]
// 参考 https://sourceware.org/binutils/docs/ld/Options.html
pub struct Opt {
    /// --build-id
    pub build_id: bool,
    /// --eh-frame-hdr
    pub eh_frame_hdr: bool,
    /// -pie
    pub pie: bool,
    /// -shared
    pub shared: bool,
    // What does the 'emulation' do in the Linker?：https://softwareengineering.stackexchange.com/questions/373269/what-does-the-emulation-do-in-the-linker
    // 简单解释了 -m 被叫做 eMulate 的历史来源。
    // 如果要查看 ld 支持的 emulations, 可以运行 `ld --verbose` 或者 `ld -V`
    /// -m emulation
    pub emulation: Option<String>,
    /// -o output
    pub output: Option<String>,
    /// -dynamic-linker
    pub dynamic_linker: Option<String>,
    /// -L searchdir
    pub search_dir: Vec<String>,
    /// --hash-style=sysv/gnu/both
    pub hash_style: HashStyle,
    /// -soname SONAME
    pub soname: Option<String>,
    /// ObjectFile
    pub obj_file: Vec<ObjectFileOpt>,
}

/// parse arguments
pub fn parse_opts(args: &[String]) -> anyhow::Result<Opt> {
    let mut opt = Opt::default();
    let mut cur_opt_stack = OptStack {
        as_needed: false,
        link_static: false,
    };
    let mut opt_stack = vec![];
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // single dash
            s if s.starts_with("-L") => {
                // library search path argument
                opt.search_dir
                    .push(s.strip_prefix("-L").unwrap().to_string());
            }
            "-dynamic-linker" => {
                // dynamic linker argument
                opt.dynamic_linker = Some(
                    iter.next()
                        .ok_or(anyhow!("Missing dynamic linker after -dynamic-linker"))?
                        .to_string(),
                );
            }
            s if s.starts_with("-l") => {
                // library argument
                opt.obj_file.push(ObjectFileOpt::Library(LibraryOpt {
                    name: s.strip_prefix("-l").unwrap().to_string(),
                    as_needed: cur_opt_stack.as_needed,
                    link_static: cur_opt_stack.link_static,
                }));
            }
            "-m" => {
                // emulation argument
                opt.emulation = Some(
                    iter.next()
                        .ok_or(anyhow!("Missing emulation after -m"))?
                        .to_string(),
                );
            }
            "-o" => {
                // output argument
                opt.output = Some(
                    iter.next()
                        .ok_or(anyhow!("Missing output after -o"))?
                        .to_string(),
                );
            }
            "-pie" => {
                opt.pie = true;
            }
            "-plugin" => {
                // skip plugin argument
                iter.next();
            }
            s if s.starts_with("-plugin-opt=") => {
                // ignored
            }
            "-shared" => {
                opt.shared = true;
            }
            "-soname" => {
                // soname argument
                opt.soname = Some(
                    iter.next()
                        .ok_or(anyhow!("Missing file name after -soname"))?
                        .to_string(),
                );
            }
            "-static" => {
                cur_opt_stack.link_static = true;
            }
            "-z" => {
                // skip -z argument for now
                iter.next();
            }

            // double dashes
            "--as-needed" => {
                cur_opt_stack.as_needed = true;
            }
            "--build-id" => {
                opt.build_id = true;
            }
            "--eh-frame-hdr" => {
                opt.eh_frame_hdr = true;
            }
            "--end-group" => {
                opt.obj_file.push(ObjectFileOpt::EndGroup);
            }
            s if s.starts_with("--hash-style=") => match s {
                "--hash-style=sysv" => {
                    opt.hash_style.sysv = true;
                    opt.hash_style.gnu = false;
                }
                "--hash-style=gnu" => {
                    opt.hash_style.sysv = false;
                    opt.hash_style.gnu = true;
                }
                "--hash-style=both" => {
                    opt.hash_style.sysv = true;
                    opt.hash_style.gnu = true;
                }
                _ => {
                    bail!("Invalid --hash-style option: {}", s)
                }
            },
            "--start-group" => {
                opt.obj_file.push(ObjectFileOpt::StartGroup);
            }
            "--pop-state" => {
                cur_opt_stack = opt_stack.pop().unwrap();
            }
            "--push-state" => {
                opt_stack.push(cur_opt_stack);
            }
            // end of known flags
            s if s.starts_with('-') => {
                // unknown flag
                return Err(anyhow!("Unknown argument: {s}"));
            }
            s => {
                // object file argument
                opt.obj_file.push(ObjectFileOpt::File(FileOpt {
                    name: s.to_string(),
                    as_needed: cur_opt_stack.as_needed,
                }));
            }
        }
    }
    Ok(opt)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_push_pop_state() {
        let opts = parse_opts(&[
            "-la".to_string(),
            "--push-state".to_string(),
            "--as-needed".to_string(),
            "-lb".to_string(),
            "--pop-state".to_string(),
            "-lc".to_string(),
        ])
        .unwrap();

        assert_eq!(opts.obj_file.len(), 3);
        if let ObjectFileOpt::Library(lib) = &opts.obj_file[0] {
            assert_eq!(lib.name, "a");
            assert!(!lib.as_needed);
        } else {
            assert!(false);
        }

        if let ObjectFileOpt::Library(lib) = &opts.obj_file[1] {
            assert_eq!(lib.name, "b");
            assert!(lib.as_needed);
        } else {
            assert!(false);
        }

        if let ObjectFileOpt::Library(lib) = &opts.obj_file[2] {
            assert_eq!(lib.name, "c");
            assert!(!lib.as_needed);
        } else {
            assert!(false);
        }
    }
}
