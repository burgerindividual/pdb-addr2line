//! Resolve addresses to function names, and to file name and line number
//! information, with the help of a PDB file. Inline stacks are supported.
//!
//! The API of this crate is intended to be similar to the API of the
//! [`addr2line` crate](https://docs.rs/addr2line/); the two [`Context`] APIs
//! have comparable functionality. This crate is for PDB files whereas `addr2line`
//! is for DWARF data (which is used in ELF and mach-o binaries, for example).
//!
//! This crate also has a [`TypeFormatter`] API which can be used to get function signature
//! strings independently from a [`Context`].
//!
//! To create a [`Context`], use [`ContextPdbData`].
//!
//! # Example
//!
//! ```
//! use pdb_addr2line::pdb;
//!
//! fn look_up_addresses<'s, S: pdb::Source<'s> + 's>(stream: S, addresses: &[u32]) -> std::result::Result<(), pdb_addr2line::Error> {
//!     let pdb = pdb::PDB::open(stream)?;
//!     let context_data = pdb_addr2line::ContextPdbData::try_from_pdb(pdb)?;
//!     let context = context_data.make_context()?;
//!
//!     for address in addresses {
//!         if let Some(procedure_frames) = context.find_frames(*address)? {
//!             eprintln!("0x{:x} - {} frames:", address, procedure_frames.frames.len());
//!             for frame in procedure_frames.frames {
//!                 let line_str = frame.line.map(|l| format!("{}", l));
//!                 eprintln!(
//!                     "     {} at {}:{}",
//!                     frame.function.as_deref().unwrap_or("<unknown>"),
//!                     frame.file.as_deref().unwrap_or("??"),
//!                     line_str.as_deref().unwrap_or("??"),
//!                 )
//!             }
//!         } else {
//!             eprintln!("{:x} - no frames found", address);
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use elsa::FrozenMap;
pub use maybe_owned;
pub use pdb;

mod error;
mod type_formatter;

pub use error::Error;
use pdb::Module;
use pdb::PublicSymbol;
use pdb::Rva;
use pdb::SymbolTable;
pub use type_formatter::*;

use maybe_owned::MaybeOwned;
use pdb::DebugInformation;
use pdb::IdInformation;
use pdb::TypeInformation;

use pdb::{
    AddressMap, FallibleIterator, FileIndex, IdIndex, InlineSiteSymbol, Inlinee, LineProgram,
    ModuleInfo, PdbInternalSectionOffset, RawString, Source, StringTable, SymbolData, SymbolIndex,
    SymbolIter, TypeIndex, PDB,
};
use range_collections::RangeSet;
use std::cmp::Ordering;
use std::collections::btree_map::Entry;
use std::collections::HashMap;
use std::ops::Bound;
use std::ops::Deref;
use std::rc::Rc;
use std::{borrow::Cow, cell::RefCell, collections::BTreeMap};

type Result<V> = std::result::Result<V, Error>;

/// Allows to easily create a [`Context`] directly from a [`pdb::PDB`].
///
/// ```
/// # fn wrapper<'s, S: pdb::Source<'s> + 's>(stream: S) -> std::result::Result<(), pdb_addr2line::Error> {
/// let pdb = pdb::PDB::open(stream)?;
/// let context_data = pdb_addr2line::ContextPdbData::try_from_pdb(pdb)?;
/// let context = context_data.make_context()?;
/// # Ok(())
/// # }
/// ```
///
/// Implementation note:
/// It would be nice if a [`Context`] could be created from a [`PDB`] directly, without
/// going through an intermediate [`ContextPdbData`] object. However, there doesn't
/// seem to be an easy way to do this, due to certain lifetime dependencies: The
/// [`Context`] object wants to store certain objects inside itself (mostly for caching)
/// which have a lifetime dependency on [`pdb::ModuleInfo`], so the [`ModuleInfo`] has to be
/// owned outside of the [`Context`]. So the [`ContextPdbData`] object acts as that external
/// [`ModuleInfo`] owner.
pub struct ContextPdbData<'s, S: Source<'s> + 's> {
    pdb: RefCell<PDB<'s, S>>,

    /// ModuleInfo objects are stored on this object (outside Context) so that the
    /// Context can internally store objects which have a lifetime dependency on
    /// ModuleInfo, such as Inlinees, LinePrograms, and RawStrings from modules.
    module_contents: FrozenMap<u16, Box<ModuleInfo<'s>>>,

    address_map: AddressMap<'s>,
    string_table: Option<StringTable<'s>>,
    global_symbols: SymbolTable<'s>,
    debug_info: DebugInformation<'s>,
    type_info: TypeInformation<'s>,
    id_info: IdInformation<'s>,
}

impl<'s, S: Source<'s> + 's> ContextPdbData<'s, S> {
    /// Create a [`ContextPdbData`] from a [`PDB`](pdb::PDB). This parses many of the PDB
    /// streams and stores them in the [`ContextPdbData`]. Most importantly, it builds
    /// a list of all the [`ModuleInfo`](pdb::ModuleInfo) objects in the PDB.
    pub fn try_from_pdb(mut pdb: PDB<'s, S>) -> Result<Self> {
        let global_symbols = pdb.global_symbols()?;
        let debug_info = pdb.debug_information()?;
        let type_info = pdb.type_information()?;
        let id_info = pdb.id_information()?;
        let address_map = pdb.address_map()?;
        let string_table = pdb.string_table().ok();

        Ok(Self {
            pdb: RefCell::new(pdb),
            module_contents: FrozenMap::new(),
            global_symbols,
            debug_info,
            type_info,
            id_info,
            address_map,
            string_table,
        })
    }

    /// Create a [`Context`]. This uses the default [`TypeFormatter`] settings.
    pub fn make_context(&self) -> Result<Context<'_, 's, '_, S>> {
        self.make_context_with_formatter_flags(Default::default())
    }

    /// Create a [`Context`], using the specified [`TypeFormatter`] flags.
    pub fn make_context_with_formatter_flags(
        &self,
        flags: TypeFormatterFlags,
    ) -> Result<Context<'_, 's, '_, S>> {
        let type_formatter =
            TypeFormatter::new(&self.debug_info, &self.type_info, &self.id_info, flags)?;

        Context::new_from_parts(
            self,
            &self.address_map,
            &self.global_symbols,
            self.string_table.as_ref(),
            &self.debug_info,
            MaybeOwned::Owned(type_formatter),
        )
    }

    fn get_module_info(
        &self,
        module_index: u16,
        module: &Module<'_>,
    ) -> Result<Option<&ModuleInfo<'s>>> {
        if let Some(m) = self.module_contents.get(&module_index) {
            return Ok(Some(m));
        }
        let mut pdb = self.pdb.borrow_mut();
        if let Some(module_info) = pdb.module_info(module)? {
            Ok(Some(
                self.module_contents
                    .insert(module_index, Box::new(module_info)),
            ))
        } else {
            Ok(None)
        }
    }
}

/// Basic information about a function.
#[derive(Clone)]
pub struct Function {
    /// The start address of the function, as a relative address (rva).
    pub start_rva: u32,
    /// The end address of the function, if known.
    pub end_rva: Option<u32>,
    /// The function name. `None` if there was an error during stringification.
    /// If this function is based on a public symbol, the consumer may need to demangle
    /// ("undecorate") the name. This can be detected based on a leading '?' byte.
    pub name: Option<String>,
}

/// The result of an address lookup from [`Context::find_frames`].
#[derive(Clone)]
pub struct FunctionFrames<'a> {
    /// The start address of the function which contained the looked-up address.
    pub start_rva: u32,
    /// The end address of the function which contained the looked-up address, if known.
    pub end_rva: Option<u32>,
    /// The inline stack at the looked-up address, ordered from inside to outside.
    /// Always contains at least one entry: the last element is always the function
    /// which contains the looked-up address.
    pub frames: Vec<Frame<'a>>,
}

/// One frame of the inline stack at the looked-up address.
#[derive(Clone)]
pub struct Frame<'a> {
    /// The function name. `None` if there was an error during stringification.
    pub function: Option<String>,
    /// The file name, if known.
    pub file: Option<Cow<'a, str>>,
    /// The line number, if known. This is the source line inside this function
    /// that is associated with the instruction at the looked-up address.
    pub line: Option<u32>,
}

/// The main API of this crate. Resolves addresses to function information.
pub struct Context<'a: 't, 's, 't, S: Source<'s> + 's> {
    context_data: &'a ContextPdbData<'s, S>,
    address_map: &'a AddressMap<'s>,
    section_contributions: Vec<ModuleSectionContribution>,
    string_table: Option<&'a StringTable<'s>>,
    type_formatter: MaybeOwned<'a, TypeFormatter<'t>>,
    modules: Vec<Module<'a>>,
    public_functions: Vec<PublicSymbolFunction<'a>>,
    module_procedures: FrozenMap<u16, Vec<ProcedureSymbolFunction<'a>>>,
    procedure_cache: RefCell<ProcedureCache>,
    extended_module_cache: RefCell<BTreeMap<u16, Rc<ExtendedModuleInfo<'a>>>>,
    inline_name_cache: RefCell<BTreeMap<IdIndex, Option<Rc<String>>>>,
    full_rva_list: RefCell<Option<Rc<Vec<u32>>>>,
}

impl<'a, 's, 't, S: Source<'s> + 's> Context<'a, 's, 't, S> {
    /// Create a [`Context`] manually. Most consumers will want to use
    /// [`ContextPdbData::make_context`] instead.
    ///
    /// However, if you interact with a PDB directly and parse some of its contents
    /// for other uses, you may want to call this method in order to avoid overhead
    /// from repeatedly parsing the same streams.
    /// TODO: This now always requires a ContextPdbData, so I've made it non-public.
    /// The reason for that is that we need a way to parse modules on-demand, and
    /// store the module outside Context so that things inside the Context can have
    /// a lifetime dependency on the module. Please let me know if you find a more
    /// elegant way to solve this.
    fn new_from_parts(
        context_data: &'a ContextPdbData<'s, S>,
        address_map: &'a AddressMap<'s>,
        global_symbols: &'a SymbolTable<'s>,
        string_table: Option<&'a StringTable<'s>>,
        debug_info: &'a DebugInformation,
        type_formatter: MaybeOwned<'a, TypeFormatter<'t>>,
    ) -> Result<Self> {
        let mut public_functions = Vec::new();

        // Start with the public function symbols.
        let mut symbol_iter = global_symbols.iter();
        while let Some(symbol) = symbol_iter.next()? {
            if let Ok(SymbolData::Public(PublicSymbol {
                function: true,
                name,
                offset,
                ..
            })) = symbol.parse()
            {
                public_functions.push(PublicSymbolFunction {
                    start_offset: offset,
                    name,
                });
            }
        }
        // Sort and de-duplicate, so that we can use binary search during lookup.
        public_functions.sort_unstable_by_key(|p| (p.start_offset.section, p.start_offset.offset));
        public_functions.dedup_by_key(|p| p.start_offset);

        // Read the section contributions. This will let us find the right module
        // based on the PdbSectionInternalOffset that corresponds to the looked-up
        // address. This allows reading module info on demand.
        let section_contributions = compute_section_contributions(debug_info)?;

        // Get the list of all modules. This only reads the list, not the actual module
        // info. To get the module info, you need to call pdb.module_info(&module), and
        // that's when the actual module stream is read. We use the list of modules so
        // that we can call pdb.module_info with the right module, which we look up based
        // on its module_index.
        // Instead of building this list upfront, we could also iterate the iterator on
        // demand.
        let mut module_iter = debug_info.modules()?;
        let mut modules = Vec::new();
        while let Some(module) = module_iter.next()? {
            modules.push(module);
        }

        Ok(Self {
            context_data,
            address_map,
            section_contributions,
            string_table,
            type_formatter,
            modules,
            public_functions,
            module_procedures: FrozenMap::new(),
            procedure_cache: RefCell::new(Default::default()),
            extended_module_cache: RefCell::new(BTreeMap::new()),
            inline_name_cache: RefCell::new(BTreeMap::new()),
            full_rva_list: RefCell::new(Default::default()),
        })
    }

    /// The number of functions found in public symbols.
    pub fn function_count(&self) -> usize {
        self.public_functions.len()
    }

    /// Iterate over all functions in the modules.
    pub fn functions(&self) -> FunctionIter<'_, 'a, 's, 't, S> {
        let mut full_rva_list = self.full_rva_list.borrow_mut();
        let full_rva_list = match &*full_rva_list {
            Some(list) => list.clone(),
            None => {
                let list = Rc::new(self.compute_full_rva_list());
                *full_rva_list = Some(list.clone());
                list
            }
        };
        FunctionIter {
            context: self,
            full_rva_list,
            cur_index: 0,
        }
    }

    /// Find the function whose code contains the provided address.
    /// The return value only contains the function name and the rva range, but
    /// no file or line information.
    pub fn find_function(&self, probe: u32) -> Result<Option<Function>> {
        let func = match self.lookup_function(probe) {
            Some(func) => func,
            None => return Ok(None),
        };

        match func {
            PublicOrProcedureSymbol::Public(func) => {
                let name = Some(func.name.to_string().to_string());
                let start_rva = match func.start_offset.to_rva(self.address_map) {
                    Some(rva) => rva.0,
                    None => return Ok(None),
                };
                Ok(Some(Function {
                    start_rva,
                    end_rva: None,
                    name,
                }))
            }
            PublicOrProcedureSymbol::Procedure(_, func) => {
                let name = self.get_procedure_name(func).map(|n| (*n).clone());
                let start_rva = match func.offset.to_rva(self.address_map) {
                    Some(rva) => rva.0,
                    None => return Ok(None),
                };
                let end_rva = start_rva + func.len;
                Ok(Some(Function {
                    start_rva,
                    end_rva: Some(end_rva),
                    name,
                }))
            }
        }
    }

    /// Find information about the source code which generated the instruction at the
    /// provided address. This information includes the function name, file name and
    /// line number, of the containing procedure and of any functions that were inlined
    /// into the procedure by the compiler, at that address.
    ///
    /// A lot of information is cached so that repeated calls are fast.
    pub fn find_frames(&self, probe: u32) -> Result<Option<FunctionFrames>> {
        let func = match self.lookup_function(probe) {
            Some(func) => func,
            None => return Ok(None),
        };

        let (module_index, proc) = match func {
            PublicOrProcedureSymbol::Public(func) => {
                let function = Some(func.name.to_string().to_string());
                let start_rva = match func.start_offset.to_rva(self.address_map) {
                    Some(rva) => rva.0,
                    None => return Ok(None),
                };
                // This is a public symbol. We only have the function name and no file / line info,
                // and no inline frames.
                return Ok(Some(FunctionFrames {
                    start_rva,
                    end_rva: None,
                    frames: vec![Frame {
                        function,
                        file: None,
                        line: None,
                    }],
                }));
            }
            PublicOrProcedureSymbol::Procedure(module_index, proc) => (module_index, proc),
        };

        let function = self.get_procedure_name(proc).map(|n| (*n).clone());
        let start_rva = match proc.offset.to_rva(self.address_map) {
            Some(rva) => rva.0,
            None => return Ok(None),
        };
        let end_rva = start_rva + proc.len;
        let module = &self.modules[module_index as usize];
        let module_info = self
            .context_data
            .get_module_info(module_index, module)
            .unwrap()
            .unwrap();
        let module = self.get_extended_module_info(module_index)?;
        let line_program = &module.line_program;
        let inlinees = &module.inlinees;

        let lines = &self.get_procedure_lines(proc, line_program)?[..];
        let search = match lines.binary_search_by_key(&probe, |li| li.start_offset) {
            Err(0) => None,
            Ok(i) => Some(i),
            Err(i) => Some(i - 1),
        };
        let (file, line) = match search {
            Some(index) => {
                let line_info = &lines[index];
                (
                    self.resolve_filename(line_program, line_info.file_index),
                    Some(line_info.line_start),
                )
            }
            None => (None, None),
        };

        let frame = Frame {
            function,
            file,
            line,
        };

        // Ordered outside to inside, until just before the end of this function.
        let mut frames = vec![frame];

        let inline_ranges = self.get_procedure_inline_ranges(module_info, proc, inlinees)?;
        let mut inline_ranges = &inline_ranges[..];

        loop {
            let current_depth = (frames.len() - 1) as u16;

            // Look up (probe, current_depth) in inline_ranges.
            // `inlined_addresses` is sorted in "breadth-first traversal order", i.e.
            // by `call_depth` first, and then by `start_offset`. See the comment at
            // the sort call for more information about why.
            let search = inline_ranges.binary_search_by(|range| {
                if range.call_depth > current_depth {
                    Ordering::Greater
                } else if range.call_depth < current_depth {
                    Ordering::Less
                } else if range.start_offset > probe {
                    Ordering::Greater
                } else if range.end_offset <= probe {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            });
            let (inline_range, remainder) = match search {
                Ok(index) => (&inline_ranges[index], &inline_ranges[index + 1..]),
                Err(_) => break,
            };
            let function = self
                .get_inline_name(inline_range.inlinee)
                .map(|name| name.deref().clone());
            let file = inline_range
                .file_index
                .and_then(|file_index| self.resolve_filename(line_program, file_index));
            let line = inline_range.line_start;
            frames.push(Frame {
                function,
                file,
                line,
            });

            inline_ranges = remainder;
        }

        // Now order from inside to outside.
        frames.reverse();

        Ok(Some(FunctionFrames {
            start_rva,
            end_rva: Some(end_rva),
            frames,
        }))
    }

    fn compute_full_rva_list(&self) -> Vec<u32> {
        let mut list = Vec::new();
        for func in &self.public_functions {
            if let Some(rva) = func.start_offset.to_rva(self.address_map) {
                list.push(rva.0);
            }
        }
        for module_index in 0..(self.modules.len() as u16) {
            if let Ok(procedures) = self.get_module_procedures(module_index) {
                for proc in procedures {
                    if let Some(rva) = proc.offset.to_rva(self.address_map) {
                        list.push(rva.0);
                    }
                }
            }
        }
        list.sort_unstable();
        list.dedup();
        list
    }

    fn get_module_procedures(&self, module_index: u16) -> Result<&[ProcedureSymbolFunction<'a>]> {
        if let Some(procedures) = self.module_procedures.get(&module_index) {
            return Ok(procedures);
        }

        let procedures = self.compute_module_procedures(module_index)?;
        Ok(self.module_procedures.insert(module_index, procedures))
    }

    fn compute_module_procedures(
        &self,
        module_index: u16,
    ) -> Result<Vec<ProcedureSymbolFunction<'a>>> {
        let module = &self.modules[module_index as usize];
        let module_info = match self.context_data.get_module_info(module_index, module)? {
            Some(m) => m,
            None => {
                return Ok(Vec::new());
            }
        };
        let mut symbols_iter = module_info.symbols()?;
        let mut functions = Vec::new();
        while let Some(symbol) = symbols_iter.next()? {
            match symbol.parse() {
                Ok(SymbolData::Procedure(proc)) => {
                    if proc.len == 0 {
                        continue;
                    }

                    let name = if proc.type_index != TypeIndex(0) {
                        // The arguments are stored in the type. Use the argument-less name,
                        // we will stringify the arguments from the type when needed.
                        proc.name
                    } else {
                        // We have no type, so proc.name might be an argument-less string.
                        // If we have a public symbol at this address which is a decorated name
                        // (starts with a '?'), prefer to use that because it'll usually include
                        // the arguments.
                        if let Ok(public_fun_index) = self
                            .public_functions
                            .binary_search_by_key(&(proc.offset.section, proc.offset.offset), |f| {
                                (f.start_offset.section, f.start_offset.offset)
                            })
                        {
                            let name = self.public_functions[public_fun_index].name;
                            if name.as_bytes().starts_with(&[b'?']) {
                                name
                            } else {
                                proc.name
                            }
                        } else {
                            proc.name
                        }
                    };

                    functions.push(ProcedureSymbolFunction {
                        offset: proc.offset,
                        len: proc.len,
                        name,
                        symbol_index: symbol.index(),
                        end_symbol_index: proc.end,
                        type_index: proc.type_index,
                    });
                }
                Ok(SymbolData::Thunk(thunk)) => {
                    if thunk.len == 0 {
                        continue;
                    }

                    // thunk.name is usually a decorated name, so it includes the arguments,
                    // and we don't have to get a name from the public functions the way we
                    // do above for procedures.

                    // Treat thunks as procedures. This isn't perfectly accurate but it
                    // doesn't cause any harm.
                    functions.push(ProcedureSymbolFunction {
                        offset: thunk.offset,
                        len: thunk.len as u32,
                        name: thunk.name,
                        symbol_index: symbol.index(),
                        end_symbol_index: thunk.end,
                        type_index: TypeIndex(0),
                    });
                }
                _ => {}
            }
        }
        // Sort and de-duplicate, so that we can use binary search during lookup.
        functions.sort_unstable_by_key(|p| (p.offset.section, p.offset.offset));
        functions.dedup_by_key(|p| p.offset);

        Ok(functions)
    }

    fn lookup_function(&self, probe: u32) -> Option<PublicOrProcedureSymbol<'_, 'a>> {
        let offset = Rva(probe).to_internal_offset(self.address_map)?;

        let sc_index = match self.section_contributions.binary_search_by(|sc| {
            if sc.section_index < offset.section {
                Ordering::Less
            } else if sc.section_index > offset.section {
                Ordering::Greater
            } else if sc.end_offset <= offset.offset {
                Ordering::Less
            } else if sc.start_offset > offset.offset {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }) {
            Ok(sc_index) => sc_index,
            Err(_) => {
                // The requested address is not present in any section contribution.
                return None;
            }
        };

        let module_index = self.section_contributions[sc_index].module_index;
        let module_procedures = self.get_module_procedures(module_index).ok()?;
        if let Ok(procedure_index) = module_procedures.binary_search_by(|p| {
            if p.offset.section < offset.section {
                Ordering::Less
            } else if p.offset.section > offset.section {
                Ordering::Greater
            } else if p.offset.offset + p.len <= offset.offset {
                Ordering::Less
            } else if p.offset.offset > offset.offset {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }) {
            // Found a procedure at the requested offset.
            return Some(PublicOrProcedureSymbol::Procedure(
                module_index,
                &module_procedures[procedure_index],
            ));
        }

        // No procedure was found at this offset in the module that the section
        // contribution pointed us at.
        // This is not uncommon.
        // Fall back to the public symbols.

        let last_public_function_starting_lte_address = match self
            .public_functions
            .binary_search_by_key(&(offset.section, offset.offset), |p| {
                (p.start_offset.section, p.start_offset.offset)
            }) {
            Err(0) => return None,
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let fun = &self.public_functions[last_public_function_starting_lte_address];
        debug_assert!(
            fun.start_offset.section < offset.section
                || (fun.start_offset.section == offset.section
                    && fun.start_offset.offset <= offset.offset)
        );
        if fun.start_offset.section != offset.section {
            return None;
        }

        Some(PublicOrProcedureSymbol::Public(fun))
    }

    fn get_extended_module_info(&self, module_index: u16) -> Result<Rc<ExtendedModuleInfo<'a>>> {
        let mut cache = self.extended_module_cache.borrow_mut();
        match cache.entry(module_index) {
            Entry::Occupied(e) => Ok(e.get().clone()),
            Entry::Vacant(e) => {
                let m = self.compute_extended_module_info(module_index)?;
                Ok(e.insert(Rc::new(m)).clone())
            }
        }
    }

    fn compute_extended_module_info(&self, module_index: u16) -> Result<ExtendedModuleInfo<'a>> {
        let module = &self.modules[module_index as usize];
        let module_info = self
            .context_data
            .get_module_info(module_index, module)
            .unwrap()
            .unwrap();
        let line_program = module_info.line_program()?;

        let inlinees: BTreeMap<IdIndex, Inlinee> = module_info
            .inlinees()?
            .map(|i| Ok((i.index(), i)))
            .collect()?;

        Ok(ExtendedModuleInfo {
            inlinees,
            line_program,
        })
    }

    fn get_procedure_name(&self, proc: &ProcedureSymbolFunction) -> Option<Rc<String>> {
        let mut cache = self.procedure_cache.borrow_mut();
        let entry = cache.get_entry_mut(proc.offset);
        match &entry.name {
            Some(name) => name.deref().clone(),
            None => {
                let name = self.compute_procedure_name(proc).map(Rc::new);
                entry.name = Some(name.clone());
                name
            }
        }
    }

    fn compute_procedure_name(&self, proc: &ProcedureSymbolFunction) -> Option<String> {
        self.type_formatter
            .format_function(&proc.name.to_string(), proc.type_index)
            .ok()
    }

    fn get_procedure_lines(
        &self,
        proc: &ProcedureSymbolFunction,
        line_program: &LineProgram,
    ) -> Result<Rc<Vec<CachedLineInfo>>> {
        let mut cache = self.procedure_cache.borrow_mut();
        let entry = cache.get_entry_mut(proc.offset);
        match &entry.lines {
            Some(lines) => Ok(lines.clone()),
            None => {
                let lines = Rc::new(self.compute_procedure_lines(proc, line_program)?);
                entry.lines = Some(lines.clone());
                Ok(lines)
            }
        }
    }

    fn compute_procedure_lines(
        &self,
        proc: &ProcedureSymbolFunction,
        line_program: &LineProgram,
    ) -> Result<Vec<CachedLineInfo>> {
        let lines_for_proc = line_program.lines_at_offset(proc.offset);
        let mut iterator = lines_for_proc.map(|line_info| {
            let rva = line_info.offset.to_rva(self.address_map).unwrap().0;
            Ok((rva, line_info))
        });
        let mut lines = Vec::new();
        let mut next_item = iterator.next()?;
        while let Some((start_offset, line_info)) = next_item {
            next_item = iterator.next()?;
            lines.push(CachedLineInfo {
                start_offset,
                file_index: line_info.file_index,
                line_start: line_info.line_start,
            });
        }
        Ok(lines)
    }

    fn get_procedure_inline_ranges(
        &self,
        module_info: &ModuleInfo,
        proc: &ProcedureSymbolFunction,
        inlinees: &BTreeMap<IdIndex, Inlinee>,
    ) -> Result<Rc<Vec<InlineRange>>> {
        let mut cache = self.procedure_cache.borrow_mut();
        let entry = cache.get_entry_mut(proc.offset);
        match &entry.inline_ranges {
            Some(inline_ranges) => Ok(inline_ranges.clone()),
            None => {
                let inline_ranges =
                    Rc::new(self.compute_procedure_inline_ranges(module_info, proc, inlinees)?);
                entry.inline_ranges = Some(inline_ranges.clone());
                Ok(inline_ranges)
            }
        }
    }

    fn compute_procedure_inline_ranges(
        &self,
        module_info: &ModuleInfo,
        proc: &ProcedureSymbolFunction,
        inlinees: &BTreeMap<IdIndex, Inlinee>,
    ) -> Result<Vec<InlineRange>> {
        let mut lines = Vec::new();
        let mut symbols_iter = module_info.symbols_at(proc.symbol_index)?;
        let _proc_sym = symbols_iter.next()?;
        while let Some(symbol) = symbols_iter.next()? {
            if symbol.index() >= proc.end_symbol_index {
                break;
            }
            match symbol.parse() {
                Ok(SymbolData::Procedure(p)) => {
                    // This is a nested procedure. Skip it.
                    symbols_iter.skip_to(p.end)?;
                }
                Ok(SymbolData::InlineSite(site)) => {
                    self.process_inlinee_symbols(
                        &mut symbols_iter,
                        inlinees,
                        proc.offset,
                        site,
                        0,
                        &mut lines,
                    )?;
                }
                _ => {}
            }
        }

        lines.sort_unstable_by(|r1, r2| {
            if r1.call_depth < r2.call_depth {
                Ordering::Less
            } else if r1.call_depth > r2.call_depth {
                Ordering::Greater
            } else if r1.start_offset < r2.start_offset {
                Ordering::Less
            } else if r1.start_offset > r2.start_offset {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });

        Ok(lines)
    }

    fn process_inlinee_symbols(
        &self,
        symbols_iter: &mut SymbolIter,
        inlinees: &BTreeMap<IdIndex, Inlinee>,
        proc_offset: PdbInternalSectionOffset,
        site: InlineSiteSymbol,
        call_depth: u16,
        lines: &mut Vec<InlineRange>,
    ) -> Result<RangeSet<u32>> {
        let mut ranges = RangeSet::empty();
        let mut file_index = None;
        if let Some(inlinee) = inlinees.get(&site.inlinee) {
            let mut iter = inlinee.lines(proc_offset, &site);
            while let Ok(Some(line_info)) = iter.next() {
                let length = match line_info.length {
                    Some(0) | None => {
                        continue;
                    }
                    Some(l) => l,
                };
                let start_offset = line_info.offset.to_rva(self.address_map).unwrap().0;
                let end_offset = start_offset + length;
                lines.push(InlineRange {
                    start_offset,
                    end_offset,
                    call_depth,
                    inlinee: site.inlinee,
                    file_index: Some(line_info.file_index),
                    line_start: Some(line_info.line_start),
                });
                ranges |= RangeSet::from(start_offset..end_offset);
                if file_index.is_none() {
                    file_index = Some(line_info.file_index);
                }
            }
        }

        let mut callee_ranges = RangeSet::empty();
        while let Some(symbol) = symbols_iter.next()? {
            if symbol.index() >= site.end {
                break;
            }
            match symbol.parse() {
                Ok(SymbolData::Procedure(p)) => {
                    // This is a nested procedure. Skip it.
                    symbols_iter.skip_to(p.end)?;
                }
                Ok(SymbolData::InlineSite(site)) => {
                    callee_ranges |= self.process_inlinee_symbols(
                        symbols_iter,
                        inlinees,
                        proc_offset,
                        site,
                        call_depth + 1,
                        lines,
                    )?;
                }
                _ => {}
            }
        }

        if !ranges.is_superset(&callee_ranges) {
            // Workaround bad debug info.
            let missing_ranges: RangeSet<u32> = &callee_ranges - &ranges;
            for range in missing_ranges.iter() {
                let (start_offset, end_offset) = match range {
                    (Bound::Included(s), Bound::Excluded(e)) => (*s, *e),
                    other => {
                        panic!("Unexpected range bounds {:?}", other);
                    }
                };
                lines.push(InlineRange {
                    start_offset,
                    end_offset,
                    call_depth,
                    inlinee: site.inlinee,
                    file_index,
                    line_start: None,
                });
            }
            ranges |= missing_ranges;
        }

        Ok(ranges)
    }

    fn get_inline_name(&self, id_index: IdIndex) -> Option<Rc<String>> {
        let mut cache = self.inline_name_cache.borrow_mut();
        cache
            .entry(id_index)
            .or_insert_with(|| match self.type_formatter.format_id(id_index) {
                Ok(name) => Some(Rc::new(name)),
                Err(_) => None,
            })
            .deref()
            .clone()
    }

    fn resolve_filename(
        &self,
        line_program: &LineProgram,
        file_index: FileIndex,
    ) -> Option<Cow<'a, str>> {
        if let Some(string_table) = self.string_table {
            if let Ok(file_info) = line_program.get_file_info(file_index) {
                return file_info.name.to_string_lossy(string_table).ok();
            }
        }
        None
    }
}

/// An iterator over all functions in a [`Context`].
#[derive(Clone)]
pub struct FunctionIter<'c, 'a, 's, 't, S: Source<'s> + 's> {
    context: &'c Context<'a, 's, 't, S>,
    full_rva_list: Rc<Vec<u32>>,
    cur_index: usize,
}

impl<'c, 'a, 's, 't, S: Source<'s> + 's> Iterator for FunctionIter<'c, 'a, 's, 't, S> {
    type Item = Function;

    fn next(&mut self) -> Option<Function> {
        loop {
            if self.cur_index >= self.full_rva_list.len() {
                return None;
            }
            let rva = self.full_rva_list[self.cur_index];
            self.cur_index += 1;
            if let Ok(Some(fun)) = self.context.find_function(rva) {
                return Some(fun);
            }
        }
    }
}

/// The order of the fields matters for the lexicographical sort.
#[derive(Debug, Clone, PartialOrd, PartialEq, Eq, Ord)]
pub struct ModuleSectionContribution {
    section_index: u16,
    start_offset: u32,
    end_offset: u32,
    module_index: u16,
}

/// Returns an array of non-overlapping `ModuleSectionContribution` objects,
/// sorted by section and then by start offset.
/// Contributions from the same module to the same section are combined into
/// one contiguous contribution. The hope is that there is no interleaving,
/// and this function returns an error if any interleaving is detected.
fn compute_section_contributions(
    debug_info: &DebugInformation<'_>,
) -> Result<Vec<ModuleSectionContribution>> {
    let mut section_contribution_iter = debug_info.section_contributions()?;
    let mut section_contributions = Vec::new();

    while let Some(first_sc) = section_contribution_iter.next()? {
        if first_sc.size == 0 {
            continue;
        }
        let mut current_combined_sc = ModuleSectionContribution {
            section_index: first_sc.offset.section,
            start_offset: first_sc.offset.offset,
            end_offset: first_sc.offset.offset + first_sc.size,
            module_index: first_sc.module,
        };
        // Assume that section contributions from the same section and module are
        // sorted and non-interleaved.
        while let Some(sc) = section_contribution_iter.next()? {
            if sc.size == 0 {
                continue;
            }
            let section_index = sc.offset.section;
            let start_offset = sc.offset.offset;
            let end_offset = start_offset + sc.size;
            let module_index = sc.module;
            if section_index == current_combined_sc.section_index
                && module_index == current_combined_sc.module_index
            {
                // Enforce ordered contributions. If you find a pdb where this errors out,
                // please file an issue.
                if end_offset < current_combined_sc.end_offset {
                    return Err(Error::UnorderedSectionContributions(
                        module_index,
                        section_index,
                    ));
                }

                // Combine with current section contribution.
                current_combined_sc.end_offset = end_offset;
            } else {
                section_contributions.push(current_combined_sc);
                current_combined_sc = ModuleSectionContribution {
                    section_index: sc.offset.section,
                    start_offset: sc.offset.offset,
                    end_offset,
                    module_index: sc.module,
                };
            }
        }
        section_contributions.push(current_combined_sc);
    }

    // Sort. This sorts by section index first, and then start offset within the section.
    section_contributions.sort_unstable();

    // Enforce no overlap. If you encounter a PDB where this errors out, please file an issue.
    if let Some((first_sc, rest)) = section_contributions.split_first() {
        let mut prev_sc = first_sc;
        for sc in rest {
            if sc.section_index == prev_sc.section_index && sc.start_offset < prev_sc.end_offset {
                return Err(Error::OverlappingSectionContributions(
                    sc.section_index,
                    prev_sc.module_index,
                    sc.module_index,
                ));
            }
            prev_sc = sc;
        }
    }

    Ok(section_contributions)
}

#[derive(Default)]
struct ProcedureCache(HashMap<PdbInternalSectionOffset, ExtendedProcedureInfo>);

impl ProcedureCache {
    fn get_entry_mut(
        &mut self,
        start_offset: PdbInternalSectionOffset,
    ) -> &mut ExtendedProcedureInfo {
        self.0
            .entry(start_offset)
            .or_insert_with(|| ExtendedProcedureInfo {
                name: None,
                lines: None,
                inline_ranges: None,
            })
    }
}

/// Offset and name of a function from a public symbol.
#[derive(Clone, Debug)]
struct PublicSymbolFunction<'s> {
    /// The address at which this function starts, as a section internal offset. The end
    /// address for global function symbols is not known. During symbol lookup, if the address
    /// is not covered by a procedure symbol (for those, the  end addresses are known), then
    /// we assume that functions with no end address cover the range up to the next function.
    start_offset: PdbInternalSectionOffset,
    /// The symbol name. This is the mangled ("decorated") function signature.
    name: RawString<'s>,
}

#[derive(Clone, Debug)]
struct ProcedureSymbolFunction<'a> {
    /// The address at which this function starts, as a section internal offset.
    offset: PdbInternalSectionOffset,
    /// The length of this function, in bytes, beginning from start_offset.
    len: u32,
    /// The symbol name. If type_index is 0, then this can be the mangled ("decorated")
    /// function signature from a PublicSymbol or from a Thunk. If type_index is non-zero,
    /// name is just the function name, potentially including class scope and namespace,
    /// but no args. The args are then found in the type.
    name: RawString<'a>,
    /// The index of the ProcedureSymbol. This allows starting a symbol iteration
    /// cheaply from this symbol, for example to find subsequent symbols about
    /// inlines in this procedure.
    symbol_index: SymbolIndex,
    /// The index of the symbol that ends this procedure. This is where the symbol
    /// iteration should stop.
    end_symbol_index: SymbolIndex,
    /// The type of this procedure, or 0. This is needed to get the arguments for the
    /// function signature.
    type_index: TypeIndex,
}

enum PublicOrProcedureSymbol<'c, 'a> {
    Public(&'c PublicSymbolFunction<'a>),
    Procedure(u16, &'c ProcedureSymbolFunction<'a>),
}

struct ExtendedProcedureInfo {
    name: Option<Option<Rc<String>>>,
    lines: Option<Rc<Vec<CachedLineInfo>>>,
    inline_ranges: Option<Rc<Vec<InlineRange>>>,
}

struct ExtendedModuleInfo<'a> {
    inlinees: BTreeMap<IdIndex, Inlinee<'a>>,
    line_program: LineProgram<'a>,
}

#[derive(Clone)]
struct CachedLineInfo {
    pub start_offset: u32,
    pub file_index: FileIndex,
    pub line_start: u32,
}

#[derive(Clone, Debug)]
struct InlineRange {
    pub start_offset: u32,
    pub end_offset: u32,
    pub call_depth: u16,
    pub inlinee: IdIndex,
    pub file_index: Option<FileIndex>,
    pub line_start: Option<u32>,
}
