use std::collections::VecDeque;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
    transform::{InnerTransform, Transformed, Transformer},
    visit::{InnerVisit, Visitor},
    AttrSet, Const, Context, DataInstForm, Func, GlobalVar, Module, ModuleDialect, Type,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    IncompatibleModuleDialects,
    MemoryModelMissmatch,
    AddressingModelMissmatch,
    VersionMissmatch { mergee: (u8, u8), merged: (u8, u8) },
    DuplicateExportKey,
}

/// A pass that merges `merged` into 'mergee'. This mostly means finding and merging
/// intersecting type declarations. Note that only _export_ points of `merged` are
/// considered when merging. Note that merging can fail if the modules are incompatible.
pub fn merge(mergee: &mut Module, merged: Module) -> Result<(), MergeError> {
    //For sanity, check that we are using the same context.
    assert!(
        std::rc::Rc::ptr_eq(merged.cx_ref(), mergee.cx_ref()),
        "mergee and merged do not have the same context"
    );

    //NOTE(siebencorgie)
    // First we need to verify some basic compatibility (spec version, memory model etc.).
    // After that we build a rewriting table for type IDs, that match `merged`'s type IDs to the `mergee`
    // IDs, or import them into `mergee` if they don't exist.
    let resolved_dialect = make_compatible(mergee.dialect.clone(), &merged.dialect)?;

    let (resolved_global_vars, resolved_funcs) = {
        let mut cpycoll = CopyCollector {
            cx: &merged.cx(),
            src_module: &merged,
            dst_module: mergee,
            seen_types: FxHashSet::default(),
            seen_consts: FxHashSet::default(),
            seen_data_inst_forms: FxHashSet::default(),
            seen_global_vars: FxHashSet::default(),
            seen_funcs: FxHashSet::default(),

            rewrite_func: FxHashMap::default(),
            rewrite_var: FxHashMap::default(),
        };

        //Visit all exportees and collect all types, const etc. which are needed to make the
        // exportee valid in the new module
        for exportee in merged.exports.values() {
            exportee.inner_visit_with(&mut cpycoll);
        }

        //Collect everything that needs to be merged
        cpycoll.visit_module(&merged);

        (cpycoll.rewrite_var, cpycoll.rewrite_func)
    };

    let mut decl_rewrite = DeclRewrite {
        cx: &mergee.cx(),
        resolved_global_vars: &resolved_global_vars,
        resolved_funcs: &resolved_funcs,

        transformed_types: FxHashMap::default(),
        transformed_consts: FxHashMap::default(),
        transformed_data_inst_forms: FxHashMap::default(),
        transformed_global_vars: FxHashMap::default(),
        global_var_queue: VecDeque::new(),
        transformed_funcs: FxHashMap::default(),
        func_queue: VecDeque::new(),
    };

    //register all exports of our just merged module.
    for (k, e) in merged.exports.iter() {
        if let Some(_exp) = mergee.exports.insert(k.clone(), e.clone()) {
            return Err(MergeError::DuplicateExportKey);
        }

        //And apply rewriting to exports
        let exportee = mergee.exports.get_mut(k).unwrap();
        exportee
            .inner_transform_with(&mut decl_rewrite)
            .apply_to(exportee);
    }

    // Process the queues until they're all empty.
    while !decl_rewrite.global_var_queue.is_empty() || !decl_rewrite.func_queue.is_empty() {
        while let Some(gv) = decl_rewrite.global_var_queue.pop_front() {
            decl_rewrite.in_place_transform_global_var_decl(&mut mergee.global_vars[gv]);
        }
        while let Some(func) = decl_rewrite.func_queue.pop_front() {
            decl_rewrite.in_place_transform_func_decl(&mut mergee.funcs[func]);
        }
    }

    //Finally apply the merged new dialect
    mergee.dialect = resolved_dialect;

    Ok(())
}

fn make_compatible(a: ModuleDialect, b: &ModuleDialect) -> Result<ModuleDialect, MergeError> {
    //NOTE(siebencorgie):
    // We currently only have spv. Not sure hot this would work otherwise
    let (a_dia, b_dia) = match (a, b) {
        (ModuleDialect::Spv(a_spv), ModuleDialect::Spv(b_spv)) => (a_spv, b_spv),
    };

    if a_dia.memory_model != b_dia.memory_model {
        return Err(MergeError::MemoryModelMissmatch);
    }
    if a_dia.addressing_model != b_dia.addressing_model {
        return Err(MergeError::AddressingModelMissmatch);
    }
    if a_dia.version_major != b_dia.version_major || a_dia.version_minor != b_dia.version_minor {
        return Err(MergeError::VersionMissmatch {
            mergee: (a_dia.version_major, a_dia.version_minor),
            merged: (b_dia.version_major, b_dia.version_minor),
        });
    }

    //since we are compatible, take the first dialect and merge any capability we don't have yet.
    // TODO(siebencorgie): Are there any incompatible capabilities we should check?
    let mut new_dialect = a_dia;
    for cap in b_dia.capabilities.iter() {
        let _ = new_dialect.capabilities.insert(cap.clone());
    }
    for ext in b_dia.extensions.iter() {
        let _ = new_dialect.extensions.insert(ext.clone());
    }

    Ok(ModuleDialect::Spv(new_dialect))
}

//Collects all functions and global variables that need to be copied over
// for all exports to become _valid_.
struct CopyCollector<'a> {
    cx: &'a Context,
    src_module: &'a Module,
    dst_module: &'a mut Module,
    seen_types: FxHashSet<Type>,
    seen_consts: FxHashSet<Const>,
    seen_data_inst_forms: FxHashSet<DataInstForm>,
    seen_global_vars: FxHashSet<GlobalVar>,
    seen_funcs: FxHashSet<Func>,

    rewrite_func: FxHashMap<Func, Func>,
    rewrite_var: FxHashMap<GlobalVar, GlobalVar>,
}
impl Visitor<'_> for CopyCollector<'_> {
    fn visit_attr_set_use(&mut self, _attrs: AttrSet) {}
    fn visit_type_use(&mut self, ty: Type) {
        if self.seen_types.insert(ty) {
            self.visit_type_def(&self.cx[ty]);
        }
    }
    fn visit_const_use(&mut self, ct: Const) {
        if self.seen_consts.insert(ct) {
            self.visit_const_def(&self.cx[ct]);
        }
    }
    fn visit_data_inst_form_use(&mut self, data_inst_form: DataInstForm) {
        if self.seen_data_inst_forms.insert(data_inst_form) {
            self.visit_data_inst_form_def(&self.cx[data_inst_form]);
        }
    }

    fn visit_global_var_use(&mut self, gv: GlobalVar) {
        if self.seen_global_vars.insert(gv) {
            self.visit_global_var_decl(&self.src_module.global_vars[gv]);
        } else {
            let nv = self
                .dst_module
                .global_vars
                .define(self.cx, self.src_module.global_vars[gv].clone());

            self.rewrite_var.insert(gv, nv);
        }
    }
    fn visit_func_use(&mut self, func: Func) {
        if self.seen_funcs.insert(func) {
            self.visit_func_decl(&self.src_module.funcs[func]);
        } else {
            //Seen the first time, therefore register in new module
            let e = self
                .dst_module
                .funcs
                .define(self.cx, self.src_module.funcs[func].clone());

            self.rewrite_func.insert(func, e);
        }
    }
}

//Uses the `resolved_*` maps to rewrite any Func/Global var to its new deceleration.
struct DeclRewrite<'a> {
    cx: &'a Context,

    resolved_global_vars: &'a FxHashMap<GlobalVar, GlobalVar>,
    resolved_funcs: &'a FxHashMap<Func, Func>,

    // FIXME(eddyb) build some automation to avoid ever repeating these.
    transformed_types: FxHashMap<Type, Transformed<Type>>,
    transformed_consts: FxHashMap<Const, Transformed<Const>>,
    transformed_data_inst_forms: FxHashMap<DataInstForm, Transformed<DataInstForm>>,
    transformed_global_vars: FxHashMap<GlobalVar, Transformed<GlobalVar>>,
    global_var_queue: VecDeque<GlobalVar>,
    transformed_funcs: FxHashMap<Func, Transformed<Func>>,
    func_queue: VecDeque<Func>,
}

impl Transformer for DeclRewrite<'_> {
    // FIXME(eddyb) build some automation to avoid ever repeating these.
    fn transform_type_use(&mut self, ty: Type) -> Transformed<Type> {
        if let Some(&cached) = self.transformed_types.get(&ty) {
            return cached;
        }
        let transformed = self
            .transform_type_def(&self.cx[ty])
            .map(|ty_def| self.cx.intern(ty_def));
        self.transformed_types.insert(ty, transformed);
        transformed
    }
    fn transform_const_use(&mut self, ct: Const) -> Transformed<Const> {
        if let Some(&cached) = self.transformed_consts.get(&ct) {
            return cached;
        }
        let transformed = self
            .transform_const_def(&self.cx[ct])
            .map(|ct_def| self.cx.intern(ct_def));
        self.transformed_consts.insert(ct, transformed);
        transformed
    }
    fn transform_data_inst_form_use(
        &mut self,
        data_inst_form: DataInstForm,
    ) -> Transformed<DataInstForm> {
        if let Some(&cached) = self.transformed_data_inst_forms.get(&data_inst_form) {
            return cached;
        }
        let transformed = self
            .transform_data_inst_form_def(&self.cx[data_inst_form])
            .map(|data_inst_form_def| self.cx.intern(data_inst_form_def));
        self.transformed_data_inst_forms
            .insert(data_inst_form, transformed);
        transformed
    }

    fn transform_global_var_use(&mut self, gv: GlobalVar) -> Transformed<GlobalVar> {
        if let Some(&cached) = self.transformed_global_vars.get(&gv) {
            return cached;
        }
        let transformed = match self.resolved_global_vars.get(&gv).copied() {
            Some(mut new_gv) => {
                self.transform_global_var_use(new_gv).apply_to(&mut new_gv);
                Transformed::Changed(new_gv)
            }
            None => {
                self.global_var_queue.push_back(gv);
                Transformed::Unchanged
            }
        };
        self.transformed_global_vars.insert(gv, transformed);
        transformed
    }
    fn transform_func_use(&mut self, func: Func) -> Transformed<Func> {
        if let Some(&cached) = self.transformed_funcs.get(&func) {
            return cached;
        }
        let transformed = match self.resolved_funcs.get(&func).copied() {
            Some(mut new_func) => {
                self.transform_func_use(new_func).apply_to(&mut new_func);
                Transformed::Changed(new_func)
            }
            None => {
                self.func_queue.push_back(func);
                Transformed::Unchanged
            }
        };
        self.transformed_funcs.insert(func, transformed);
        transformed
    }
}