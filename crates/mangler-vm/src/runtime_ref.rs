//! JavaScript environment references used by dynamic object scopes.
//!
//! A resolver is retained by closures, but each expression materializes its
//! reference before evaluation continues. This separates environment lookup from
//! GetValue/PutValue and preserves mutation order around getters and assignments.

pub(crate) fn helpers(used: impl Fn(usize) -> bool) -> String {
    let mut source = String::new();
    if (62..=70).any(&used) || used(72) || used(36) || (83..=85).any(&used) {
        source.push_str(r#"function ReferenceOwn(o,k){return Object.prototype.hasOwnProperty.call(o,k)?o[k]:undefined;}"#);
    }
    if (62..=70).any(&used) || used(72) || used(36) || (83..=85).any(&used) {
        source.push_str(
            r#"function ResolveReference(r){return ReferenceOwn(r,"resolve")?r.resolve():r;}"#,
        );
    }
    if used(62) || used(36) {
        source.push_str(r#"function LocalReference(n,c){
 if(c)return {get:function(){return L[n][0];},set:function(v){L[n][0]=v;},type:function(){return typeof L[n][0];},del:function(){return false;},receiver:undefined};
 var d=Object.getOwnPropertyDescriptor(L,n),g=d&&ReferenceOwn(d,"get");
 if(g&&ReferenceOwn(g,"vmResolve"))return {resolve:g.vmResolve};
 return {get:g||function(){return L[n];},set:function(v,s){var put=s&&g&&ReferenceOwn(g,"vmStrictSet")||d&&ReferenceOwn(d,"set");if(put)put(v);else if(g||d&&d.writable===false){if(s)throw TypeError('Assignment to immutable binding');}else L[n]=v;},
 type:g&&ReferenceOwn(g,"vmType")||(g?function(){return typeof g();}:function(){return typeof L[n];}),del:g&&ReferenceOwn(g,"vmDelete")||function(){return false;},receiver:undefined};
}"#);
    }
    if used(63) || used(85) {
        source.push_str(r#"function PropertyReference(o,k){
 function get(s){if(k in o)return o[k];if(s)throw ReferenceError(k+' is not defined');return undefined;}
 return {get:get,set:function(v,s){var exists=k in o;if(!exists&&s)throw ReferenceError(k+' is not defined');if(!Reflect.set(o,k,v,o)&&s)throw TypeError('Cannot assign object binding');},
 type:function(s){return typeof get(s);},del:function(){return Reflect.deleteProperty(o,k);},receiver:o};
}"#);
    }
    if used(63) || used(85) {
        source.push_str(r#"function WithReference(r,o,k){
 return {resolve:function(){
  if(k in o){var u=o[Symbol.unscopables];if(u===null||Object(u)!==u||!u[k])return PropertyReference(o,k);}
  return ResolveReference(r);
 }};
}"#);
    }
    if used(66) || used(68) || used(72) || used(84) {
        source.push_str(r#"function SetReference(r,v){r.set(v,(function(){return this===undefined;})());return v;}"#);
    }
    if used(36) || used(68) {
        source.push_str(r#"function ReferenceDescriptor(reference,s){
 var getter=function(){return ResolveReference(reference).get(s);};
 getter.vmResolve=function(){return ResolveReference(reference);};
 getter.vmCapture=function(strict){return ReferenceDescriptor(reference,strict);};
 getter.vmType=function(){return ResolveReference(reference).type(s);};
 getter.vmDelete=function(){return ResolveReference(reference).del();};
 return {__proto__:null,configurable:true,get:getter,set:function(v){ResolveReference(reference).set(v,s);},type:getter.vmType,del:getter.vmDelete};
}"#);
    }
    if used(36) {
        source.push_str(r#"function NativeReference(n){
 var x={},binding=LocalReference(n,0);
 function view(s){var d=ReferenceDescriptor(binding,s);Object.defineProperty(x,s?2:0,d);
 Object.defineProperty(x,s?3:1,{get:function(){var r=ResolveReference(binding),f=r.get(s);return f===null||f===undefined?f:function(){return Reflect.apply(f,r.receiver,arguments);};}});}
 view(false);view(true);return x;
}"#);
    }
    if used(68) {
        source.push_str(r#"function CaptureReference(n,r){Object.defineProperty(L,n,ReferenceDescriptor(r,(function(){return this===undefined;})()));}"#);
    }
    if used(83) {
        source.push_str(r#"function AccessorReference(n,boxed){
 var cell=L[n];if(boxed&&cell!==undefined)cell=cell[0];
 function value(){if(cell===undefined)throw ReferenceError('Uninitialized lexical binding');return cell;}
 return {get:function(){return value().v;},set:function(v){value().v=v;},type:function(){return typeof value().v;},del:function(){return false;},receiver:undefined};
}"#);
    }
    if used(84) {
        source.push_str(r#"function ReferenceAdapter(r){
 var adapter={};
 Object.defineProperty(adapter,'v',{get:function(){return r.get((function(){return this===undefined;})());},set:function(v){SetReference(r,v);}});
 Object.defineProperty(adapter,'c',{get:function(){var f=r.get((function(){return this===undefined;})());return f===null||f===undefined?f:function(){return Reflect.apply(f,r.receiver,arguments);};}});
 Object.defineProperty(adapter,'d',{get:function(){return r.del();}});
 return adapter;
}"#);
    }
    if used(84) && used(81) {
        source = source.replace(
            "get:function(){var f=r.get((function(){return this===undefined;})());return f===null",
            "get:function(){var f=r.get((function(){return this===undefined;})());if(f===Programs.evalIntrinsic)return f;return f===null",
        );
    }
    if used(81) {
        source = source.replace(
            "Reflect.apply(f,r.receiver,arguments)",
            "SourceInvoke(f,r.receiver,arguments)",
        );
    }
    source
}

pub(crate) fn handler(op: usize) -> &'static str {
    match op {
        62 => "n=C[pc++];Push(S,LocalReference(n>>>1,n&1));break;",
        63 => "k=consts[C[pc++]];n=C[pc++];v=Pop(S);Push(S,WithReference(v,L[n],k));break;",
        64 => "v=Pop(S);Push(S,ResolveReference(v));break;",
        65 => {
            "v=ResolveReference(Pop(S));Push(S,v.get((function(){return this===undefined;})()));break;"
        }
        66 => "v=Pop(S);o=ResolveReference(Pop(S));SetReference(o,v);Push(S,v);break;",
        67 => {
            "v=ResolveReference(Pop(S));f=v.get((function(){return this===undefined;})());Push(S,v.receiver);Push(S,f);break;"
        }
        68 => "n=C[pc++];v=Pop(S);CaptureReference(n,v);break;",
        69 => "v=ResolveReference(Pop(S));Push(S,v.del());break;",
        70 => {
            "v=ResolveReference(Pop(S));Push(S,v.type((function(){return this===undefined;})()));break;"
        }
        71 => {
            "n=C[pc++];v=Pop(S);if(v===null||v===undefined)throw TypeError('Cannot enter null object scope');L[n]=Object(v);break;"
        }
        72 => {
            "n=C[pc++];o=ResolveReference(Pop(S));v=o.get((function(){return this===undefined;})());a=n&2?v--:v++;SetReference(o,v);Push(S,n&1?v:a);break;"
        }
        75 => {
            "n=C[pc++];Object.defineProperty(L,n,{value:undefined,writable:true,configurable:true});break;"
        }
        83 => "n=C[pc++];Push(S,AccessorReference(n>>>1,n&1));break;",
        84 => "v=ResolveReference(Pop(S));Push(S,ReferenceAdapter(v));break;",
        85 => "k=consts[C[pc++]];n=C[pc++];v=Pop(S);Push(S,WithReference(v,L[n][0],k));break;",
        _ => "",
    }
}

#[cfg(test)]
mod object_binding_read_tests {
    /// ECMA-262 9.1.1.2.6 is the oracle: native engines may omit observable
    /// repeated Proxy.has checks in object environments, so differential
    /// execution of the original with statement is insufficient here.
    #[test]
    fn get_binding_value_checks_property_and_consumer_strictness() {
        use mangler_testkit::eval::{CaptureMode, assert_behaviorally_equal_with};
        let helpers = super::helpers(|_| true);
        let environment = crate::runtime_env::helpers(|_| true);
        let fixture = include_str!("../tests/fixtures/object_binding_reads.js");
        let cases: String = [65, 67, 70, 72]
            .into_iter()
            .map(|op| format!("case {op}:{}", super::handler(op)))
            .collect();
        let machine = format!(
            "var S=[ref],C=[0],pc=0,v,f,o,n,a;function Pop(s){{return s.pop()}}function Push(s,v){{s.push(v)}}switch(op){{{cases}}}return S[S.length-1];"
        );
        let transformed = format!(
            "var L=[],Programs={{}},SourceInvoke=Reflect.apply;{helpers}{environment}function sloppyOpcode(op,ref){{{machine}}}function strictOpcode(op,ref){{'use strict';{machine}}}{fixture}"
        );
        assert_behaviorally_equal_with(
            "globalThis.__out='42';",
            &transformed,
            &CaptureMode::sink(),
        );
    }
}
