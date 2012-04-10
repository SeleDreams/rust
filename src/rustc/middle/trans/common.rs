/**
   Code that is useful in various trans modules.

*/

import libc::c_uint;
import vec::unsafe::to_ptr;
import std::map::{hashmap,set};
import syntax::ast;
import driver::session;
import session::session;
import middle::{resolve, ty};
import back::{link, abi, upcall};
import util::common::*;
import syntax::codemap::span;
import lib::llvm::{llvm, target_data, type_names, associate_type,
                   name_has_type};
import lib::llvm::{ModuleRef, ValueRef, TypeRef, BasicBlockRef, BuilderRef};
import lib::llvm::{True, False, Bool};
import metadata::csearch;
import ast_map::path;

type namegen = fn@(str) -> str;
fn new_namegen() -> namegen {
    let i = @mut 0;
    ret fn@(prefix: str) -> str { *i += 1; prefix + int::str(*i) };
}

type tydesc_info =
    {ty: ty::t,
     tydesc: ValueRef,
     size: ValueRef,
     align: ValueRef,
     mut take_glue: option<ValueRef>,
     mut drop_glue: option<ValueRef>,
     mut free_glue: option<ValueRef>};

/*
 * A note on nomenclature of linking: "upcall", "extern" and "native".
 *
 * An "extern" is an LLVM symbol we wind up emitting an undefined external
 * reference to. This means "we don't have the thing in this compilation unit,
 * please make sure you link it in at runtime". This could be a reference to
 * C code found in a C library, or rust code found in a rust crate.
 *
 * A "native" is an extern that references C code. Called with cdecl.
 *
 * An upcall is a native call generated by the compiler (not corresponding to
 * any user-written call in the code) into librustrt, to perform some helper
 * task such as bringing a task to life, allocating memory, etc.
 *
 */

type stats =
    {mut n_static_tydescs: uint,
     mut n_glues_created: uint,
     mut n_null_glues: uint,
     mut n_real_glues: uint,
     llvm_insn_ctxt: @mut [str],
     llvm_insns: hashmap<str, uint>,
     fn_times: @mut [{ident: str, time: int}]};

resource BuilderRef_res(B: BuilderRef) { llvm::LLVMDisposeBuilder(B); }

// Misc. auxiliary maps used in the crate_ctxt
type maps = {
    mutbl_map: middle::mutbl::mutbl_map,
    copy_map: middle::alias::copy_map,
    last_uses: middle::last_use::last_uses,
    impl_map: middle::resolve::impl_map,
    method_map: middle::typeck::method_map,
    vtable_map: middle::typeck::vtable_map,
    spill_map: last_use::spill_map
};

// Crate context.  Every crate we compile has one of these.
type crate_ctxt = {
     sess: session::session,
     llmod: ModuleRef,
     td: target_data,
     tn: type_names,
     externs: hashmap<str, ValueRef>,
     intrinsics: hashmap<str, ValueRef>,
     item_vals: hashmap<ast::node_id, ValueRef>,
     exp_map: resolve::exp_map,
     reachable: reachable::map,
     item_symbols: hashmap<ast::node_id, str>,
     mut main_fn: option<ValueRef>,
     link_meta: link::link_meta,
     enum_sizes: hashmap<ty::t, uint>,
     discrims: hashmap<ast::def_id, ValueRef>,
     discrim_symbols: hashmap<ast::node_id, str>,
     tydescs: hashmap<ty::t, @tydesc_info>,
     // Track mapping of external ids to local items imported for inlining
     external: hashmap<ast::def_id, option<ast::node_id>>,
     // Cache instances of monomorphized functions
     monomorphized: hashmap<mono_id, ValueRef>,
     // Cache computed type parameter uses (see type_use.rs)
     type_use_cache: hashmap<ast::def_id, [type_use::type_uses]>,
     // Cache generated vtables
     vtables: hashmap<mono_id, ValueRef>,
     module_data: hashmap<str, ValueRef>,
     lltypes: hashmap<ty::t, TypeRef>,
     names: namegen,
     sha: std::sha1::sha1,
     type_sha1s: hashmap<ty::t, str>,
     type_short_names: hashmap<ty::t, str>,
     all_llvm_symbols: set<str>,
     tcx: ty::ctxt,
     maps: maps,
     stats: stats,
     upcalls: @upcall::upcalls,
     tydesc_type: TypeRef,
     int_type: TypeRef,
     float_type: TypeRef,
     task_type: TypeRef,
     opaque_vec_type: TypeRef,
     builder: BuilderRef_res,
     shape_cx: shape::ctxt,
     crate_map: ValueRef,
     dbg_cx: option<debuginfo::debug_ctxt>,
     // Mapping from class constructors to parent class --
     // used in base::trans_closure
     // parent_class must be a def_id because ctors can be
     // inlined, so the parent may be in a different crate
     class_ctors: hashmap<ast::node_id, ast::def_id>,
     mut do_not_commit_warning_issued: bool};

// Types used for llself.
type val_self_pair = {v: ValueRef, t: ty::t};

enum local_val { local_mem(ValueRef), local_imm(ValueRef), }

type param_substs = {tys: [ty::t],
                     vtables: option<typeck::vtable_res>,
                     bounds: @[ty::param_bounds]};

// Function context.  Every LLVM function we create will have one of
// these.
type fn_ctxt = @{
    // The ValueRef returned from a call to llvm::LLVMAddFunction; the
    // address of the first instruction in the sequence of
    // instructions for this function that will go in the .text
    // section of the executable we're generating.
    llfn: ValueRef,

    // The two implicit arguments that arrive in the function we're creating.
    // For instance, foo(int, int) is really foo(ret*, env*, int, int).
    llenv: ValueRef,
    llretptr: ValueRef,

    // These elements: "hoisted basic blocks" containing
    // administrative activities that have to happen in only one place in
    // the function, due to LLVM's quirks.
    // A block for all the function's static allocas, so that LLVM
    // will coalesce them into a single alloca call.
    mut llstaticallocas: BasicBlockRef,
    // A block containing code that copies incoming arguments to space
    // already allocated by code in one of the llallocas blocks.
    // (LLVM requires that arguments be copied to local allocas before
    // allowing most any operation to be performed on them.)
    mut llloadenv: BasicBlockRef,
    mut llreturn: BasicBlockRef,
    // The 'self' value currently in use in this function, if there
    // is one.
    mut llself: option<val_self_pair>,
    // The a value alloca'd for calls to upcalls.rust_personality. Used when
    // outputting the resume instruction.
    mut personality: option<ValueRef>,
    // If this is a for-loop body that returns, this holds the pointers needed
    // for that
    mut loop_ret: option<{flagptr: ValueRef, retptr: ValueRef}>,

    // Maps arguments to allocas created for them in llallocas.
    llargs: hashmap<ast::node_id, local_val>,
    // Maps the def_ids for local variables to the allocas created for
    // them in llallocas.
    lllocals: hashmap<ast::node_id, local_val>,
    // Same as above, but for closure upvars
    llupvars: hashmap<ast::node_id, ValueRef>,

    // The node_id of the function, or -1 if it doesn't correspond to
    // a user-defined function.
    id: ast::node_id,

    // If this function is being monomorphized, this contains the type
    // substitutions used.
    param_substs: option<param_substs>,

    // The source span and nesting context where this function comes from, for
    // error reporting and symbol generation.
    span: option<span>,
    path: path,

    // This function's enclosing crate context.
    ccx: @crate_ctxt
};

fn warn_not_to_commit(ccx: @crate_ctxt, msg: str) {
    if !ccx.do_not_commit_warning_issued {
        ccx.do_not_commit_warning_issued = true;
        ccx.sess.warn(msg + " -- do not commit like this!");
    }
}

enum cleantype {
    normal_exit_only,
    normal_exit_and_unwind
}

enum cleanup {
    clean(fn@(block) -> block, cleantype),
    clean_temp(ValueRef, fn@(block) -> block, cleantype),
}

// Used to remember and reuse existing cleanup paths
// target: none means the path ends in an resume instruction
type cleanup_path = {target: option<BasicBlockRef>,
                     dest: BasicBlockRef};

fn scope_clean_changed(info: scope_info) {
    if info.cleanup_paths.len() > 0u { info.cleanup_paths = []; }
    info.landing_pad = none;
}

fn cleanup_type(cx: ty::ctxt, ty: ty::t) -> cleantype {
    if ty::type_needs_unwind_cleanup(cx, ty) {
        normal_exit_and_unwind
    } else {
        normal_exit_only
    }
}

fn add_clean(cx: block, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(cx.tcx(), ty) { ret; }
    let cleanup_type = cleanup_type(cx.tcx(), ty);
    in_scope_cx(cx) {|info|
        info.cleanups += [clean(bind base::drop_ty(_, val, ty),
                                cleanup_type)];
        scope_clean_changed(info);
    }
}
fn add_clean_temp(cx: block, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(cx.tcx(), ty) { ret; }
    let cleanup_type = cleanup_type(cx.tcx(), ty);
    fn do_drop(bcx: block, val: ValueRef, ty: ty::t) ->
       block {
        if ty::type_is_immediate(ty) {
            ret base::drop_ty_immediate(bcx, val, ty);
        } else {
            ret base::drop_ty(bcx, val, ty);
        }
    }
    in_scope_cx(cx) {|info|
        info.cleanups += [clean_temp(val, bind do_drop(_, val, ty),
                                     cleanup_type)];
        scope_clean_changed(info);
    }
}
fn add_clean_temp_mem(cx: block, val: ValueRef, ty: ty::t) {
    if !ty::type_needs_drop(cx.tcx(), ty) { ret; }
    let cleanup_type = cleanup_type(cx.tcx(), ty);
    in_scope_cx(cx) {|info|
        info.cleanups += [clean_temp(val, bind base::drop_ty(_, val, ty),
                                     cleanup_type)];
        scope_clean_changed(info);
    }
}
fn add_clean_free(cx: block, ptr: ValueRef, shared: bool) {
    let free_fn = if shared { bind base::trans_shared_free(_, ptr) }
                  else { bind base::trans_free(_, ptr) };
    in_scope_cx(cx) {|info|
        info.cleanups += [clean_temp(ptr, free_fn,
                                     normal_exit_and_unwind)];
        scope_clean_changed(info);
    }
}

// Note that this only works for temporaries. We should, at some point, move
// to a system where we can also cancel the cleanup on local variables, but
// this will be more involved. For now, we simply zero out the local, and the
// drop glue checks whether it is zero.
fn revoke_clean(cx: block, val: ValueRef) {
    in_scope_cx(cx) {|info|
        option::iter(vec::position(info.cleanups, {|cu|
            alt cu { clean_temp(v, _, _) if v == val { true } _ { false } }
        })) {|i|
            info.cleanups =
                vec::slice(info.cleanups, 0u, i) +
                vec::slice(info.cleanups, i + 1u, info.cleanups.len());
            scope_clean_changed(info);
        }
    }
}

enum block_kind {
    // A scope at the end of which temporary values created inside of it are
    // cleaned up. May correspond to an actual block in the language, but also
    // to an implicit scope, for example, calls introduce an implicit scope in
    // which the arguments are evaluated and cleaned up.
    block_scope(scope_info),
    // A non-scope block is a basic block created as a translation artifact
    // from translating code that expresses conditional logic rather than by
    // explicit { ... } block structure in the source language.  It's called a
    // non-scope block because it doesn't introduce a new variable scope.
    block_non_scope,
}

enum loop_cont { cont_self, cont_other(block), }

type scope_info = {
    is_loop: option<{cnt: loop_cont, brk: block}>,
    // A list of functions that must be run at when leaving this
    // block, cleaning up any variables that were introduced in the
    // block.
    mut cleanups: [cleanup],
    // Existing cleanup paths that may be reused, indexed by destination and
    // cleared when the set of cleanups changes.
    mut cleanup_paths: [cleanup_path],
    // Unwinding landing pad. Also cleared when cleanups change.
    mut landing_pad: option<BasicBlockRef>,
};

// Basic block context.  We create a block context for each basic block
// (single-entry, single-exit sequence of instructions) we generate from Rust
// code.  Each basic block we generate is attached to a function, typically
// with many basic blocks per function.  All the basic blocks attached to a
// function are organized as a directed graph.
type block = @{
    // The BasicBlockRef returned from a call to
    // llvm::LLVMAppendBasicBlock(llfn, name), which adds a basic
    // block to the function pointed to by llfn.  We insert
    // instructions into that block by way of this block context.
    // The block pointing to this one in the function's digraph.
    llbb: BasicBlockRef,
    mut terminated: bool,
    mut unreachable: bool,
    parent: block_parent,
    // The 'kind' of basic block this is.
    kind: block_kind,
    // The source span where the block came from, if it is a block that
    // actually appears in the source code.
    mut block_span: option<span>,
    // The function context for the function to which this block is
    // attached.
    fcx: fn_ctxt
};

// First two args are retptr, env
const first_real_arg: uint = 2u;

// FIXME move blocks to a class once those are finished, and simply use
// option<block> for this.
enum block_parent { parent_none, parent_some(block), }

type result = {bcx: block, val: ValueRef};
type result_t = {bcx: block, val: ValueRef, ty: ty::t};

fn rslt(bcx: block, val: ValueRef) -> result {
    {bcx: bcx, val: val}
}

fn ty_str(tn: type_names, t: TypeRef) -> str {
    ret lib::llvm::type_to_str(tn, t);
}

fn val_ty(&&v: ValueRef) -> TypeRef { ret llvm::LLVMTypeOf(v); }

fn val_str(tn: type_names, v: ValueRef) -> str { ret ty_str(tn, val_ty(v)); }

// Returns the nth element of the given LLVM structure type.
fn struct_elt(llstructty: TypeRef, n: uint) -> TypeRef unsafe {
    let elt_count = llvm::LLVMCountStructElementTypes(llstructty) as uint;
    assert (n < elt_count);
    let elt_tys = vec::from_elem(elt_count, T_nil());
    llvm::LLVMGetStructElementTypes(llstructty, to_ptr(elt_tys));
    ret llvm::LLVMGetElementType(elt_tys[n]);
}

fn in_scope_cx(cx: block, f: fn(scope_info)) {
    let mut cur = cx;
    loop {
        alt cur.kind {
          block_scope(info) { f(info); ret; }
          _ {}
        }
        cur = block_parent(cur);
    }
}

fn block_parent(cx: block) -> block {
    alt check cx.parent { parent_some(b) { b } }
}

// Accessors

impl bxc_cxs for block {
    fn ccx() -> @crate_ctxt { self.fcx.ccx }
    fn tcx() -> ty::ctxt { self.fcx.ccx.tcx }
    fn sess() -> session { self.fcx.ccx.sess }
}

// LLVM type constructors.
fn T_void() -> TypeRef {
    // Note: For the time being llvm is kinda busted here, it has the notion
    // of a 'void' type that can only occur as part of the signature of a
    // function, but no general unit type of 0-sized value. This is, afaict,
    // vestigial from its C heritage, and we'll be attempting to submit a
    // patch upstream to fix it. In the mean time we only model function
    // outputs (Rust functions and C functions) using T_void, and model the
    // Rust general purpose nil type you can construct as 1-bit (always
    // zero). This makes the result incorrect for now -- things like a tuple
    // of 10 nil values will have 10-bit size -- but it doesn't seem like we
    // have any other options until it's fixed upstream.

    ret llvm::LLVMVoidType();
}

fn T_nil() -> TypeRef {
    // NB: See above in T_void().

    ret llvm::LLVMInt1Type();
}

fn T_metadata() -> TypeRef { ret llvm::LLVMMetadataType(); }

fn T_i1() -> TypeRef { ret llvm::LLVMInt1Type(); }

fn T_i8() -> TypeRef { ret llvm::LLVMInt8Type(); }

fn T_i16() -> TypeRef { ret llvm::LLVMInt16Type(); }

fn T_i32() -> TypeRef { ret llvm::LLVMInt32Type(); }

fn T_i64() -> TypeRef { ret llvm::LLVMInt64Type(); }

fn T_f32() -> TypeRef { ret llvm::LLVMFloatType(); }

fn T_f64() -> TypeRef { ret llvm::LLVMDoubleType(); }

fn T_bool() -> TypeRef { ret T_i1(); }

fn T_int(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86 { T_i32() }
      session::arch_x86_64 { T_i64() }
      session::arch_arm { T_i32() }
    };
}

fn T_int_ty(cx: @crate_ctxt, t: ast::int_ty) -> TypeRef {
    alt t {
      ast::ty_i { cx.int_type }
      ast::ty_char { T_char() }
      ast::ty_i8 { T_i8() }
      ast::ty_i16 { T_i16() }
      ast::ty_i32 { T_i32() }
      ast::ty_i64 { T_i64() }
    }
}

fn T_uint_ty(cx: @crate_ctxt, t: ast::uint_ty) -> TypeRef {
    alt t {
      ast::ty_u { cx.int_type }
      ast::ty_u8 { T_i8() }
      ast::ty_u16 { T_i16() }
      ast::ty_u32 { T_i32() }
      ast::ty_u64 { T_i64() }
    }
}

fn T_float_ty(cx: @crate_ctxt, t: ast::float_ty) -> TypeRef {
    alt t {
      ast::ty_f { cx.float_type }
      ast::ty_f32 { T_f32() }
      ast::ty_f64 { T_f64() }
    }
}

fn T_float(targ_cfg: @session::config) -> TypeRef {
    ret alt targ_cfg.arch {
      session::arch_x86 { T_f64() }
      session::arch_x86_64 { T_f64() }
      session::arch_arm { T_f64() }
    };
}

fn T_char() -> TypeRef { ret T_i32(); }

fn T_size_t(targ_cfg: @session::config) -> TypeRef {
    ret T_int(targ_cfg);
}

fn T_fn(inputs: [TypeRef], output: TypeRef) -> TypeRef unsafe {
    ret llvm::LLVMFunctionType(output, to_ptr(inputs),
                               inputs.len() as c_uint,
                               False);
}

fn T_fn_pair(cx: @crate_ctxt, tfn: TypeRef) -> TypeRef {
    ret T_struct([T_ptr(tfn), T_opaque_cbox_ptr(cx)]);
}

fn T_ptr(t: TypeRef) -> TypeRef {
    ret llvm::LLVMPointerType(t, 0u as c_uint);
}

fn T_struct(elts: [TypeRef]) -> TypeRef unsafe {
    ret llvm::LLVMStructType(to_ptr(elts), elts.len() as c_uint, False);
}

fn T_named_struct(name: str) -> TypeRef {
    let c = llvm::LLVMGetGlobalContext();
    ret str::as_c_str(name, {|buf| llvm::LLVMStructCreateNamed(c, buf) });
}

fn set_struct_body(t: TypeRef, elts: [TypeRef]) unsafe {
    llvm::LLVMStructSetBody(t, to_ptr(elts),
                            elts.len() as c_uint, False);
}

fn T_empty_struct() -> TypeRef { ret T_struct([]); }

// A vtable is, in reality, a vtable pointer followed by zero or more pointers
// to tydescs and other vtables that it closes over. But the types and number
// of those are rarely known to the code that needs to manipulate them, so
// they are described by this opaque type.
fn T_vtable() -> TypeRef { T_array(T_ptr(T_i8()), 1u) }

fn T_task(targ_cfg: @session::config) -> TypeRef {
    let t = T_named_struct("task");

    // Refcount
    // Delegate pointer
    // Stack segment pointer
    // Runtime SP
    // Rust SP
    // GC chain


    // Domain pointer
    // Crate cache pointer

    let t_int = T_int(targ_cfg);
    let elems =
        [t_int, t_int, t_int, t_int,
         t_int, t_int, t_int, t_int];
    set_struct_body(t, elems);
    ret t;
}

fn T_tydesc_field(cx: @crate_ctxt, field: int) -> TypeRef unsafe {
    // Bit of a kludge: pick the fn typeref out of the tydesc..

    let tydesc_elts: [TypeRef] =
        vec::from_elem::<TypeRef>(abi::n_tydesc_fields as uint,
                                 T_nil());
    llvm::LLVMGetStructElementTypes(cx.tydesc_type,
                                    to_ptr::<TypeRef>(tydesc_elts));
    let t = llvm::LLVMGetElementType(tydesc_elts[field]);
    ret t;
}

fn T_glue_fn(cx: @crate_ctxt) -> TypeRef {
    let s = "glue_fn";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_tydesc_field(cx, abi::tydesc_field_drop_glue);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_tydesc(targ_cfg: @session::config) -> TypeRef {
    let tydesc = T_named_struct("tydesc");
    let tydescpp = T_ptr(T_ptr(tydesc));
    let pvoid = T_ptr(T_i8());
    let glue_fn_ty =
        T_ptr(T_fn([T_ptr(T_nil()), T_ptr(T_nil()), tydescpp,
                    pvoid], T_void()));

    let int_type = T_int(targ_cfg);
    let elems =
        [tydescpp, int_type, int_type,
         glue_fn_ty, glue_fn_ty, glue_fn_ty,
         T_ptr(T_i8()), glue_fn_ty, glue_fn_ty, glue_fn_ty, T_ptr(T_i8()),
         T_ptr(T_i8()), T_ptr(T_i8()), int_type, int_type];
    set_struct_body(tydesc, elems);
    ret tydesc;
}

fn T_array(t: TypeRef, n: uint) -> TypeRef {
    ret llvm::LLVMArrayType(t, n as c_uint);
}

// Interior vector.
//
// FIXME: Support user-defined vector sizes.
fn T_vec2(targ_cfg: @session::config, t: TypeRef) -> TypeRef {
    ret T_struct([T_int(targ_cfg), // fill
                  T_int(targ_cfg), // alloc
                  T_array(t, 0u)]); // elements
}

fn T_vec(ccx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_vec2(ccx.sess.targ_cfg, t);
}

// Note that the size of this one is in bytes.
fn T_opaque_vec(targ_cfg: @session::config) -> TypeRef {
    ret T_vec2(targ_cfg, T_i8());
}

// Let T be the content of a box @T.  tuplify_box_ty(t) returns the
// representation of @T as a tuple (i.e., the ty::t version of what T_box()
// returns).
fn tuplify_box_ty(tcx: ty::ctxt, t: ty::t) -> ty::t {
    ret tuplify_cbox_ty(tcx, t, ty::mk_type(tcx));
}

// As tuplify_box_ty(), but allows the caller to specify what type of type
// descr is embedded in the box (ty::type vs ty::send_type).  This is useful
// for unique closure boxes, hence the name "cbox_ty" (closure box type).
fn tuplify_cbox_ty(tcx: ty::ctxt, t: ty::t, tydesc_t: ty::t) -> ty::t {
    let ptr = ty::mk_ptr(tcx, {ty: ty::mk_nil(tcx), mutbl: ast::m_imm});
    ret ty::mk_tup(tcx, [ty::mk_uint(tcx), tydesc_t,
                         ptr, ptr,
                         t]);
}

fn T_box_header_fields(cx: @crate_ctxt) -> [TypeRef] {
    let ptr = T_ptr(T_i8());
    ret [cx.int_type, T_ptr(cx.tydesc_type), ptr, ptr];
}

fn T_box_header(cx: @crate_ctxt) -> TypeRef {
    ret T_struct(T_box_header_fields(cx));
}

fn T_box(cx: @crate_ctxt, t: TypeRef) -> TypeRef {
    ret T_struct(T_box_header_fields(cx) + [t]);
}

fn T_opaque_box(cx: @crate_ctxt) -> TypeRef {
    ret T_box(cx, T_i8());
}

fn T_opaque_box_ptr(cx: @crate_ctxt) -> TypeRef {
    ret T_ptr(T_opaque_box(cx));
}

fn T_port(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_chan(cx: @crate_ctxt, _t: TypeRef) -> TypeRef {
    ret T_struct([cx.int_type]); // Refcount

}

fn T_taskptr(cx: @crate_ctxt) -> TypeRef { ret T_ptr(cx.task_type); }


// This type must never be used directly; it must always be cast away.
fn T_typaram(tn: type_names) -> TypeRef {
    let s = "typaram";
    alt name_has_type(tn, s) { some(t) { ret t; } _ {} }
    let t = T_i8();
    associate_type(tn, s, t);
    ret t;
}

fn T_typaram_ptr(tn: type_names) -> TypeRef { ret T_ptr(T_typaram(tn)); }

fn T_opaque_cbox_ptr(cx: @crate_ctxt) -> TypeRef {
    // closures look like boxes (even when they are fn~ or fn&)
    // see trans_closure.rs
    ret T_opaque_box_ptr(cx);
}

fn T_enum_variant(cx: @crate_ctxt) -> TypeRef {
    ret cx.int_type;
}

fn T_enum(cx: @crate_ctxt, size: uint) -> TypeRef {
    let s = "enum_" + uint::to_str(size, 10u);
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t =
        if size == 0u {
            T_struct([T_enum_variant(cx)])
        } else { T_struct([T_enum_variant(cx), T_array(T_i8(), size)]) };
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_enum(cx: @crate_ctxt) -> TypeRef {
    let s = "opaque_enum";
    alt name_has_type(cx.tn, s) { some(t) { ret t; } _ {} }
    let t = T_struct([T_enum_variant(cx), T_i8()]);
    associate_type(cx.tn, s, t);
    ret t;
}

fn T_opaque_enum_ptr(cx: @crate_ctxt) -> TypeRef {
    ret T_ptr(T_opaque_enum(cx));
}

fn T_captured_tydescs(cx: @crate_ctxt, n: uint) -> TypeRef {
    ret T_struct(vec::from_elem::<TypeRef>(n, T_ptr(cx.tydesc_type)));
}

fn T_opaque_iface(cx: @crate_ctxt) -> TypeRef {
    T_struct([T_ptr(cx.tydesc_type), T_opaque_box_ptr(cx)])
}

fn T_opaque_port_ptr() -> TypeRef { ret T_ptr(T_i8()); }

fn T_opaque_chan_ptr() -> TypeRef { ret T_ptr(T_i8()); }


// LLVM constant constructors.
fn C_null(t: TypeRef) -> ValueRef { ret llvm::LLVMConstNull(t); }

fn C_integral(t: TypeRef, u: u64, sign_extend: Bool) -> ValueRef {
    let u_hi = (u >> 32u64) as c_uint;
    let u_lo = u as c_uint;
    ret llvm::LLVMRustConstInt(t, u_hi, u_lo, sign_extend);
}

fn C_floating(s: str, t: TypeRef) -> ValueRef {
    ret str::as_c_str(s, {|buf| llvm::LLVMConstRealOfString(t, buf) });
}

fn C_nil() -> ValueRef {
    // NB: See comment above in T_void().

    ret C_integral(T_i1(), 0u64, False);
}

fn C_bool(b: bool) -> ValueRef {
    C_integral(T_bool(), if b { 1u64 } else { 0u64 }, False)
}

fn C_i32(i: i32) -> ValueRef {
    ret C_integral(T_i32(), i as u64, True);
}

fn C_i64(i: i64) -> ValueRef {
    ret C_integral(T_i64(), i as u64, True);
}

fn C_int(cx: @crate_ctxt, i: int) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, True);
}

fn C_uint(cx: @crate_ctxt, i: uint) -> ValueRef {
    ret C_integral(cx.int_type, i as u64, False);
}

fn C_u8(i: uint) -> ValueRef { ret C_integral(T_i8(), i as u64, False); }


// This is a 'c-like' raw string, which differs from
// our boxed-and-length-annotated strings.
fn C_cstr(cx: @crate_ctxt, s: str) -> ValueRef {
    let sc = str::as_c_str(s) {|buf|
        llvm::LLVMConstString(buf, str::len(s) as c_uint, False)
    };
    let g =
        str::as_c_str(cx.names("str"),
                    {|buf| llvm::LLVMAddGlobal(cx.llmod, val_ty(sc), buf) });
    llvm::LLVMSetInitializer(g, sc);
    llvm::LLVMSetGlobalConstant(g, True);
    lib::llvm::SetLinkage(g, lib::llvm::InternalLinkage);
    ret g;
}

// Returns a Plain Old LLVM String:
fn C_postr(s: str) -> ValueRef {
    ret str::as_c_str(s) {|buf|
        llvm::LLVMConstString(buf, str::len(s) as c_uint, False)
    };
}

fn C_zero_byte_arr(size: uint) -> ValueRef unsafe {
    let mut i = 0u;
    let mut elts: [ValueRef] = [];
    while i < size { elts += [C_u8(0u)]; i += 1u; }
    ret llvm::LLVMConstArray(T_i8(), vec::unsafe::to_ptr(elts),
                             elts.len() as c_uint);
}

fn C_struct(elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstStruct(vec::unsafe::to_ptr(elts),
                              elts.len() as c_uint, False);
}

fn C_named_struct(T: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstNamedStruct(T, vec::unsafe::to_ptr(elts),
                                   elts.len() as c_uint);
}

fn C_array(ty: TypeRef, elts: [ValueRef]) -> ValueRef unsafe {
    ret llvm::LLVMConstArray(ty, vec::unsafe::to_ptr(elts),
                             elts.len() as c_uint);
}

fn C_bytes(bytes: [u8]) -> ValueRef unsafe {
    ret llvm::LLVMConstString(
        unsafe::reinterpret_cast(vec::unsafe::to_ptr(bytes)),
        bytes.len() as c_uint, False);
}

fn C_shape(ccx: @crate_ctxt, bytes: [u8]) -> ValueRef {
    let llshape = C_bytes(bytes);
    let llglobal = str::as_c_str(ccx.names("shape"), {|buf|
        llvm::LLVMAddGlobal(ccx.llmod, val_ty(llshape), buf)
    });
    llvm::LLVMSetInitializer(llglobal, llshape);
    llvm::LLVMSetGlobalConstant(llglobal, True);
    lib::llvm::SetLinkage(llglobal, lib::llvm::InternalLinkage);
    ret llvm::LLVMConstPointerCast(llglobal, T_ptr(T_i8()));
}

fn get_param(fndecl: ValueRef, param: uint) -> ValueRef {
    llvm::LLVMGetParam(fndecl, param as c_uint)
}

// Used to identify cached monomorphized functions and vtables
enum mono_param_id {
    mono_precise(ty::t, option<[mono_id]>),
    mono_any,
    mono_repr(uint /* size */, uint /* align */),
}
type mono_id = @{def: ast::def_id, params: [mono_param_id]};
fn hash_mono_id(&&mi: mono_id) -> uint {
    let mut h = syntax::ast_util::hash_def_id(mi.def);
    for vec::each(mi.params) {|param|
        h = h * alt param {
          mono_precise(ty, vts) {
            let mut h = ty::type_id(ty);
            option::iter(vts) {|vts|
                for vec::each(vts) {|vt| h += hash_mono_id(vt); }
            }
            h
          }
          mono_any { 1u }
          mono_repr(sz, align) { sz * (align + 2u) }
        }
    }
    h
}

fn umax(cx: block, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    ret build::Select(cx, cond, b, a);
}

fn umin(cx: block, a: ValueRef, b: ValueRef) -> ValueRef {
    let cond = build::ICmp(cx, lib::llvm::IntULT, a, b);
    ret build::Select(cx, cond, a, b);
}

fn align_to(cx: block, off: ValueRef, align: ValueRef) -> ValueRef {
    let mask = build::Sub(cx, align, C_int(cx.ccx(), 1));
    let bumped = build::Add(cx, off, mask);
    ret build::And(cx, bumped, build::Not(cx, mask));
}

fn path_str(p: path) -> str {
    let mut r = "", first = true;
    for vec::each(p) {|e|
        alt e { ast_map::path_name(s) | ast_map::path_mod(s) {
          if first { first = false; }
          else { r += "::"; }
          r += s;
        } }
    }
    r
}

fn node_id_type(bcx: block, id: ast::node_id) -> ty::t {
    let tcx = bcx.tcx();
    let t = ty::node_id_to_type(tcx, id);
    alt bcx.fcx.param_substs {
      some(substs) { ty::substitute_type_params(tcx, substs.tys, t) }
      _ { assert !ty::type_has_params(t); t }
    }
}
fn expr_ty(bcx: block, ex: @ast::expr) -> ty::t {
    node_id_type(bcx, ex.id)
}
fn node_id_type_params(bcx: block, id: ast::node_id) -> [ty::t] {
    let tcx = bcx.tcx();
    let params = ty::node_id_to_type_params(tcx, id);
    alt bcx.fcx.param_substs {
      some(substs) {
        vec::map(params) {|t| ty::substitute_type_params(tcx, substs.tys, t) }
      }
      _ { params }
    }
}

fn field_idx_strict(cx: ty::ctxt, sp: span, ident: ast::ident,
                    fields: [ty::field])
    -> int {
    alt ty::field_idx(ident, fields) {
            none { cx.sess.span_bug(sp, #fmt("base expr doesn't appear to \
                     have a field named %s", ident)); }
            some(i) { i as int }
        }
}

//
// Local Variables:
// mode: rust
// fill-column: 78;
// indent-tabs-mode: nil
// c-basic-offset: 4
// buffer-file-coding-system: utf-8-unix
// End:
//
