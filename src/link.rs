use crate::opt::{FileOpt, ObjectFileOpt, Opt};
use anyhow::{anyhow, Context};
use log::{info, warn};
use object::{
    write::{elf::SectionIndex, StringId},
    Object, ObjectSection, ObjectSymbol,
};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf};
use typed_arena::Arena;

fn lookup_file(name: &str, paths: &Vec<String>) -> anyhow::Result<PathBuf> {
    for path in paths {
        let mut p = PathBuf::from(path);
        p.push(name);
        if p.is_file() {
            info!("File {name} is found at {}", p.display());
            return Ok(p);
        }
    }
    Err(anyhow!("File {name} cannot be found"))
}

/// Resolve library namespec to paths
pub fn path_resolution(opt: &Opt) -> anyhow::Result<Opt> {
    // resolve library to actual files
    let mut opt = opt.clone();
    for obj_file in &mut opt.obj_file {
        // convert ObjectFileOpt::Library to ObjectFileOpt::File
        if let ObjectFileOpt::Library(lib) = obj_file {
            if !lib.link_static {
                // lookup dynamic library first
                let path = format!("lib{}.so", lib.name);
                if let Ok(path) = lookup_file(&path, &opt.search_dir) {
                    *obj_file = ObjectFileOpt::File(FileOpt {
                        name: format!("{}", path.display()),
                        as_needed: lib.as_needed,
                    });
                    continue;
                }
            }

            // lookup static library
            let path = format!("lib{}.a", lib.name);
            let path = lookup_file(&path, &opt.search_dir)?;
            *obj_file = ObjectFileOpt::File(FileOpt {
                name: format!("{}", path.display()),
                as_needed: lib.as_needed,
            });
            continue;
        }
    }
    Ok(opt)
}

#[derive(Debug, Clone)]
pub struct ObjectFile {
    pub name: String,
    /// --as-needed
    pub as_needed: bool,
    pub content: Vec<u8>,
}

// we want our own Relocation & RelocationTarget struct for easier handling
#[derive(Debug)]
pub enum RelocationTarget {
    // relocation against section with additional offset
    Section((String, u64)),
    // relocation against symbol
    Symbol(String),
}

#[derive(Debug)]
pub struct Relocation {
    // offset into the output section
    offset: u64,
    inner: object::Relocation,
    target: RelocationTarget,
}

#[derive(Debug)]
pub struct Symbol {
    section_name: String,
    // offset into the output section
    offset: u64,
    // indices in output ELF
    symbol_name_string_id: Option<StringId>,
}

#[derive(Default, Debug)]
pub struct OutputSection {
    pub name: String,
    pub content: Vec<u8>,
    // offset from ELF load address
    pub offset: u64,
    // relocations in this section
    pub relocations: Vec<Relocation>,
    pub is_executable: bool,
    // indices in output ELF
    pub section_index: Option<SectionIndex>,
    pub name_string_id: Option<StringId>,
}

/// Do the actual linking
pub fn link(opt: &Opt) -> anyhow::Result<()> {
    info!("Link with options: {opt:?}");

    let opt = path_resolution(&opt)?;
    info!("Options after path resolution: {opt:?}");

    // read files
    let mut files = vec![];
    for obj_file in &opt.obj_file {
        match obj_file {
            ObjectFileOpt::File(file_opt) => {
                info!("Reading {}", file_opt.name);
                files.push(ObjectFile {
                    name: file_opt.name.clone(),
                    as_needed: file_opt.as_needed,
                    content: std::fs::read(&file_opt.name)
                        .context(format!("Reading file {}", file_opt.name))?,
                });
            }
            ObjectFileOpt::Library(_) => unreachable!("Path resolution is not working"),
            ObjectFileOpt::StartGroup => warn!("--start-group unhandled"),
            ObjectFileOpt::EndGroup => warn!("--end-group unhandled"),
        }
    }

    // section name => section
    let mut output_sections: BTreeMap<String, OutputSection> = BTreeMap::new();

    // symbol table: symbol name => symbol
    let mut symbols: BTreeMap<String, Symbol> = BTreeMap::new();

    // parse files and resolve symbols
    for file in &files {
        info!("Parsing {}", file.name);
        if file.name.ends_with(".a") {
            // archive
            let _ar = object::read::archive::ArchiveFile::parse(file.content.as_slice())
                .context(format!("Parsing file {} as archive", file.name))?;
        } else {
            // object
            let obj = object::File::parse(file.content.as_slice())
                .context(format!("Parsing file {} as object", file.name))?;
            match obj {
                object::File::Elf64(elf) => {
                    // collect section sizes prior to this object
                    let section_sizes: BTreeMap<String, u64> = output_sections
                        .iter()
                        .map(|(key, value)| (key.clone(), value.content.len() as u64))
                        .collect();

                    for section in elf.sections() {
                        let name = section.name()?;
                        info!("Handling section {}", name);
                        let data = section.data()?;
                        if data.is_empty() {
                            continue;
                        }

                        if ![".symtab", ".strtab", ".shstrtab"].contains(&name)
                            && !name.starts_with(".rela")
                            && !name.is_empty()
                        {
                            // copy to output
                            let out = output_sections
                                .entry(name.to_string())
                                .or_insert_with(OutputSection::default);
                            out.name = name.to_string();
                            out.content.extend(data);
                            for (offset, relocation) in section.relocations() {
                                match relocation.target() {
                                    object::RelocationTarget::Symbol(symbol_id) => {
                                        let symbol = elf.symbol_by_index(symbol_id)?;
                                        if symbol.kind() == object::SymbolKind::Section {
                                            // relocation to a section
                                            let section_index = symbol.section_index().unwrap();
                                            let target_section =
                                                elf.section_by_index(section_index)?;
                                            let target_section_name = target_section.name()?;
                                            info!(
                                                "Found relocation target to section {}",
                                                target_section_name
                                            );

                                            out.relocations.push(Relocation {
                                                offset: offset
                                                    + *section_sizes.get(name).unwrap_or(&0),
                                                inner: relocation,
                                                target: RelocationTarget::Section((
                                                    target_section_name.to_string(),
                                                    // record current size of section, because there can be existing content in the section from other object file
                                                    *section_sizes
                                                        .get(target_section_name)
                                                        .unwrap_or(&0),
                                                )),
                                            });
                                        } else {
                                            // relocation to a symbol
                                            let symbol_name = symbol.name()?;
                                            info!(
                                                "Found relocation target to symbol {}",
                                                symbol_name
                                            );

                                            out.relocations.push(Relocation {
                                                offset: offset
                                                    + *section_sizes.get(name).unwrap_or(&0),
                                                inner: relocation,
                                                target: RelocationTarget::Symbol(
                                                    symbol_name.to_string(),
                                                ),
                                            });
                                        }
                                    }
                                    _ => unimplemented!(),
                                };
                            }

                            match section.flags() {
                                object::SectionFlags::Elf { sh_flags } => {
                                    out.is_executable |=
                                        ((sh_flags as u32) & object::elf::SHF_EXECINSTR) != 0;
                                }
                                _ => unimplemented!(),
                            }
                        }
                    }

                    // skip the first symbol which is null
                    for symbol in elf.symbols().skip(1) {
                        if !symbol.is_undefined() && symbol.kind() != object::SymbolKind::Section {
                            let name = symbol.name()?;
                            match symbol.section() {
                                object::SymbolSection::Section(section_index) => {
                                    let section = elf.section_by_index(section_index)?;
                                    let section_name = section.name()?;
                                    info!("Defining symbol {} from section {}", name, section_name);
                                    // offset: consider existing section content from other files
                                    let offset = symbol.address()
                                        + section_sizes.get(section_name).unwrap_or(&0);
                                    symbols.insert(
                                        name.to_string(),
                                        Symbol {
                                            section_name: section_name.to_string(),
                                            offset: offset,
                                            symbol_name_string_id: None,
                                        },
                                    );
                                }
                                _ => unimplemented!(),
                            }
                        }
                    }
                }
                _ => return Err(anyhow!("Unsupported format of file {}", file.name)),
            }
        }
    }

    // create executable ELF
    use object::write::elf::*;
    let mut buffer = vec![];
    let mut writer = Writer::new(object::Endianness::Little, true, &mut buffer);

    // assign address to output sections
    // and generate layout of executable
    // assume executable is loaded at 0x400000
    // the first page is reserved for ELF header & program header
    writer.reserve_file_header();
    // for simplicity, use one segment to map them all
    writer.reserve_program_headers(1);

    // thus sections begin at 0x401000
    let load_address = 0x400000;
    for (_name, output_section) in &mut output_sections {
        output_section.offset = writer.reserve(output_section.content.len(), 4096) as u64;
    }
    info!("Output sections: {:?}", output_sections);

    // reserve section headers
    writer.reserve_null_section_index();
    // use typed-arena to avoid borrow to `output_sections`
    let arena: Arena<u8> = Arena::new();
    for (name, output_section) in &mut output_sections {
        output_section.name_string_id =
            Some(writer.add_section_name(arena.alloc_str(&name).as_bytes()));
        output_section.section_index = Some(writer.reserve_section_index());
    }
    let _symtab_section_index = writer.reserve_symtab_section_index();
    let _strtab_section_index = writer.reserve_strtab_section_index();
    let _shstrtab_section_index = writer.reserve_shstrtab_section_index();
    writer.reserve_section_headers();

    // prepare symbol table
    writer.reserve_null_symbol_index();
    for (symbol_name, symbol) in &mut symbols {
        symbol.symbol_name_string_id =
            Some(writer.add_string(arena.alloc_str(&symbol_name).as_bytes()));
        writer.reserve_symbol_index(None);
    }

    // reserve symtab, strtab and shstrtab
    writer.reserve_symtab();
    writer.reserve_strtab();
    writer.reserve_shstrtab();

    // compute mapping from section name to virtual address
    let mut section_address: BTreeMap<String, u64> = BTreeMap::new();
    for (name, output_section) in &mut output_sections {
        section_address.insert(name.clone(), output_section.offset + load_address);
    }

    // compute relocation
    for (name, output_section) in &mut output_sections {
        for relocation in &output_section.relocations {
            info!("Handling relocation {:?} from section {}", relocation, name);
            let target_address = match &relocation.target {
                RelocationTarget::Section((name, offset)) => {
                    info!("Handling relocation target to section {}", name);
                    section_address[name] + offset
                }
                RelocationTarget::Symbol(name) => {
                    info!("Handling relocation target to symbol {}", name);
                    let symbol = &symbols[name];
                    section_address[&symbol.section_name] + symbol.offset
                }
            };

            match (
                relocation.inner.kind(),
                relocation.inner.encoding(),
                relocation.inner.size(),
            ) {
                // R_X86_64_32S
                (object::RelocationKind::Absolute, object::RelocationEncoding::X86Signed, 32) => {
                    info!("Handling relocation R_X86_64_32S");
                    // S + A
                    let value = target_address as i64 + relocation.inner.addend();
                    output_section.content
                        [(relocation.offset) as usize..(relocation.offset + 4) as usize]
                        .copy_from_slice(&(value as i32).to_le_bytes());
                }
                // R_X86_64_PLT32
                (object::RelocationKind::PltRelative, object::RelocationEncoding::Generic, 32) => {
                    info!("Handling relocation R_X86_64_PLT32");
                    // we don't have PLT now, implement as R_X86_64_PC32
                    // S + A - P
                    let mut value = target_address as i64 + relocation.inner.addend();
                    value -= (load_address + output_section.offset + relocation.offset) as i64;

                    output_section.content
                        [(relocation.offset) as usize..(relocation.offset + 4) as usize]
                        .copy_from_slice(&(value as i32).to_le_bytes());
                }
                _ => unimplemented!("Unimplemented relocation {:?}", relocation),
            }
        }
    }

    // all set! we can now write actual data to buffer
    // compute entrypoint address
    let entry_symbol = &symbols["_start"];
    let entry_address = section_address[&entry_symbol.section_name] + entry_symbol.offset;

    // ELF header
    writer.write_file_header(&FileHeader {
        os_abi: 0,
        abi_version: 0,
        e_type: object::elf::ET_EXEC,
        e_machine: object::elf::EM_X86_64,
        // assume that entrypoint is pointed at _start
        e_entry: entry_address,
        e_flags: 0,
    })?;
    // program header
    // ask kernel to load segments into memory
    writer.write_program_header(&ProgramHeader {
        p_type: object::elf::PT_LOAD,
        p_flags: object::elf::PF_X | object::elf::PF_W | object::elf::PF_R,
        p_offset: 0,
        p_vaddr: load_address as u64,
        p_paddr: load_address as u64,
        p_filesz: writer.reserved_len() as u64,
        p_memsz: writer.reserved_len() as u64,
        p_align: 4096,
    });

    // write section data
    for (_name, output_section) in &mut output_sections {
        writer.pad_until(output_section.offset as usize);
        writer.write(&output_section.content);
    }

    // write section headers
    writer.write_null_section_header();
    for (name, output_section) in &mut output_sections {
        let mut flags = object::elf::SHF_ALLOC;
        if output_section.is_executable {
            flags |= object::elf::SHF_EXECINSTR;
        }

        writer.write_section_header(&SectionHeader {
            name: output_section.name_string_id,
            sh_type: object::elf::SHT_PROGBITS,
            sh_flags: flags as u64,
            sh_addr: section_address[name],
            sh_offset: output_section.offset,
            sh_size: output_section.content.len() as u64,
            sh_link: 0,
            sh_info: 0,
            sh_addralign: 1,
            sh_entsize: 0,
        });
    }
    writer.write_symtab_section_header(1); // one local: null symbol
    writer.write_strtab_section_header();
    writer.write_shstrtab_section_header();

    // write symbol table
    writer.write_null_symbol();
    for (_symbol_name, symbol) in &symbols {
        let address = section_address[&symbol.section_name] + symbol.offset;
        writer.write_symbol(&Sym {
            name: symbol.symbol_name_string_id,
            section: output_sections[&symbol.section_name].section_index,
            st_info: (object::elf::STB_GLOBAL) << 4,
            st_other: 0,
            st_shndx: 0,
            st_value: address,
            st_size: 0,
        });
    }

    // write string table
    writer.write_strtab();

    // write section string table
    writer.write_shstrtab();

    // done, save to file
    let output = opt.output.as_ref().unwrap();
    info!("Writing to executable {:?}", output);
    std::fs::write(output, buffer)?;

    // make executable
    let mut perms = std::fs::metadata(output)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(output, perms)?;

    Ok(())
}
