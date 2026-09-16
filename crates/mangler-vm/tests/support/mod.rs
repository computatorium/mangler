//! Entry wrappers for direct compiler tests. Use the same native arguments
//! factory and parameter-reference ABI as VM-created closures.
pub fn entry(
    table: &str,
    chunk: &mangler_vm::chunk::Chunk,
    name: &str,
    captures: &str,
    live: bool,
    length: u32,
) -> String {
    let index = chunk.index;
    let cap_start = chunk.cap_start;
    let pcount = chunk.pcount;
    format!(
        "var {name}=(function(__vm_row){{\
         var __vm_invoke=function(__vm_recv,__vm_args,__vm_refs,__vm_target){{\
         return __vm_row[2](__vm_row[0],__vm_row[1],__vm_args,{captures},{cap_start},{pcount},__vm_recv,{live},__vm_refs,__vm_target);}};\
         return __vm_row[5]?__vm_row[5](__vm_invoke):__vm_row[3]?\
         function(){{'use strict';return __vm_invoke(this,arguments,undefined,new.target);}}:\
         function(){{return __vm_invoke(this,arguments,undefined,new.target);}};\
         }})({table}[{index}]);\
         Object.defineProperty({name},'name',{{value:{name:?},configurable:true}});\
         Object.defineProperty({name},'length',{{value:{length},configurable:true}});\
         var f={name};"
    )
}

/// ECMAScript function length excludes the first default and all following
/// parameters; a destructuring parameter without a default contributes one.
pub fn function_length(params: &[swc_core::ecma::ast::Param]) -> u32 {
    use swc_core::ecma::ast::Pat;
    params
        .iter()
        .take_while(|param| !matches!(param.pat, Pat::Assign(_) | Pat::Rest(_)))
        .count() as u32
}
