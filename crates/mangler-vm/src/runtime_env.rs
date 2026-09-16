//! Activation records shared by bytecode closures and runtime-compiled eval.
//! Every dictionary stores references, so evaluation preserves lexical TDZ,
//! const writes, object-scope receivers, and bindings introduced after capture.

pub(crate) fn helpers(used: impl Fn(usize) -> bool) -> String {
    if !(79..=82).any(used) {
        return String::new();
    }
    r#"
function EnvironmentBinding(n,flags,late,name,objects){
 var r;
 if(flags&4){
  var saved=late?undefined:L[n];
  var alias=function(){var value=saved===undefined?L[n]:saved;if((flags&1)&&value!==undefined)value=value[0];if(value===undefined)throw ReferenceError('Uninitialized lexical binding');return value;};
  r={get:function(){return alias().v;},set:function(v){alias().v=v;},type:function(){return typeof alias().v;},del:function(){return false;},receiver:undefined};
 }
 else if((flags&1)&&late)r=LocalReference(n,1);
 else if(flags&1){var cell=L[n];r={get:function(){return cell[0];},set:function(v){cell[0]=v;},type:function(){return typeof cell[0];},del:function(){return false;},receiver:undefined};}
 else r=LocalReference(n,0);
 var base=r;
 if(objects)for(var i=0;i<objects.length;i++){var entry=objects[i],object=L[entry[0]];r=WithReference(r,entry[1]?object[0]:object,name);}
 return {r:r,base:base,a:!!(flags&4),l:!!(flags&2)};
}
function EnvironmentBindings(items,late){
 var b=Object.create(null);
 for(var j=0;j<items.length;j++){var x=items[j];b[consts[x[0]]]=EnvironmentBinding(x[1],x[2],late,consts[x[0]],ReferenceOwn(x,3));}
 return b;
}
function JoinEnvironment(parent,tail){
 if(!parent)return tail;
 var head={b:parent.b,w:ReferenceOwn(parent,"w"),v:ReferenceOwn(parent,"v"),g:ReferenceOwn(parent,"g")},node=head;
 for(var e=parent.p;e;e=e.p){node.p={b:e.b,w:ReferenceOwn(e,"w"),v:ReferenceOwn(e,"v"),g:ReferenceOwn(e,"g")};node=node.p;}
 node.p=tail;return head;
}
function CaptureEnvironment(meta,parent){
 var e=null,scopes=meta.e;
 for(var j=scopes.length-1;j>=0;j--){var scope=scopes[j];
  if(scope===0)e=JoinEnvironment(parent,e);
  else if(Array.isArray(scope))e={b:EnvironmentBindings(scope),p:e,v:false,g:false,w:undefined};
  else e={w:L[scope.w],p:e,v:false,g:false,b:null};
 }
 return e;
}
function BeginVariableEnvironment(meta,parent){
 var b=Object.create(null),scopes=meta.e;
 for(var j=0;j<scopes.length;j++)if(Array.isArray(scopes[j])){
  var a=scopes[j];for(var k=0;k<a.length;k++){var x=a[k];b[consts[x[0]]]=EnvironmentBinding(x[1],x[2],true,consts[x[0]],ReferenceOwn(x,3));}
 }
 return {b:b,p:parent,v:true,g:false,w:undefined};
}
function EnvironmentReference(name,environment,fallback){
 return {resolve:function(){
  for(var e=environment;e;e=e.p){
   if(Object.prototype.hasOwnProperty.call(e,"w")&&e.w!==undefined){var o=e.w;if(name in o){var u=o[Symbol.unscopables];if(u===null||Object(u)!==u||!u[name])return PropertyReference(o,name);}}
   else if(e.b&&Object.prototype.hasOwnProperty.call(e.b,name))return ResolveReference(e.b[name].r);
  }
  return ResolveReference(fallback);
 }};
}
function EnvironmentGlobalReference(name){
 if(ReferenceOwn(Programs,"ambient"))return Programs.ambient(name);
 return {get:function(){if(!(name in globalThis))throw ReferenceError(name+' is not defined');return globalThis[name];},
 set:function(v,s){if(s&&!(name in globalThis))throw ReferenceError(name+' is not defined');if(!Reflect.set(globalThis,name,v,globalThis)&&s)throw TypeError('Cannot assign global binding');},
 type:function(){return typeof globalThis[name];},del:function(){return Reflect.deleteProperty(globalThis,name);},receiver:undefined};
}
function CheckGlobalEvalBinding(name,functionDeclaration){
 var d=Object.getOwnPropertyDescriptor(globalThis,name);
 if(!d){if(!Object.isExtensible(globalThis))throw TypeError('Cannot declare global binding '+name);return;}
 if(functionDeclaration&&!d.configurable&&!(Object.prototype.hasOwnProperty.call(d,'value')&&d.writable&&d.enumerable))throw TypeError('Cannot declare global function '+name);
}
function DeclareEvalBinding(record,name,functionDeclaration,existingReference){
 if(record.g){
  var d=Object.getOwnPropertyDescriptor(globalThis,name);
  if(!d||(functionDeclaration&&d.configurable))Object.defineProperty(globalThis,name,{value:undefined,writable:true,enumerable:true,configurable:true});
  record.b[name]={l:false,r:EnvironmentGlobalReference(name)};return;
 }
 if(existingReference){record.b[name]={l:false,r:existingReference};return;}
 var value;
 record.b[name]={l:false,r:{get:function(){return value;},set:function(v){value=v;},type:function(){return typeof value;},del:function(){return delete record.b[name];},receiver:undefined}};
}
function EvaluateInEnvironment(meta,parent,recv,fn,argv,callerThis,target){
 if(fn!==Programs.evalIntrinsic)return SourceInvoke(fn,recv,argv);
 var source=argv[0];if(typeof source!=='string')return source;
 var env=CaptureEnvironment(meta,parent),strict=ReferenceOwn(meta,"i")?false:(function(){return this===undefined;})();
 var classMeta=ReferenceOwn(meta,"k"),capsule=classMeta?L[classMeta.capsuleSlot]:undefined;
 if(classMeta&&classMeta.capsuleCell)capsule=capsule[0];
 var grammar=classMeta?{privateNames:classMeta.privateNames,allowSuperProperty:classMeta.allowSuperProperty,allowSuperCall:classMeta.allowSuperCall,argumentsForbidden:classMeta.argumentsForbidden}:undefined;
 var compiled=Programs.eval(source,{strict:strict,allowNewTarget:meta.c===0,sourceContext:meta.c,classContext:grammar},env),names=compiled.declaredVars,functions=compiled.declaredFunctions||[];
 for(var i=0;i<names.length;i++){
  var name=names[i],record=env;
  while(record&&!record.v){if(record.b&&Object.prototype.hasOwnProperty.call(record.b,name)&&record.b[name].l)throw SyntaxError('Eval declaration conflicts with lexical binding '+name);record=record.p;}
  if(!record)throw ReferenceError('Missing eval variable environment');
  if(Object.prototype.hasOwnProperty.call(record.b,name)){if(record.b[name].l)throw SyntaxError('Eval declaration conflicts with lexical binding '+name);}
  if(record.g){var isFunction=false;for(var j=0;j<functions.length;j++)if(functions[j]===name){isFunction=true;break;}CheckGlobalEvalBinding(name,isFunction);}
 }
 for(var i=0;i<names.length;i++){
  var name=names[i],record=env,existingReference=undefined;while(record&&!record.v){if(!existingReference&&record.b&&Object.prototype.hasOwnProperty.call(record.b,name)&&record.b[name].a&&!record.b[name].l)existingReference=record.b[name].base||record.b[name].r;record=record.p;}
  var isFunction=false;for(var j=0;j<functions.length;j++)if(functions[j]===name){isFunction=true;break;}
  if(record.g||!Object.prototype.hasOwnProperty.call(record.b,name))DeclareEvalBinding(record,name,isFunction,existingReference);
 }
 var caps=compiled.program.captures,up=List(),classCaptures=ReferenceOwn(compiled,"classCaptures");
 for(var i=0;i<caps.length;i++){
  var isClassCapture=false;if(classCaptures)for(var j=0;j<classCaptures.length;j++)if(classCaptures[j]===caps[i]){isClassCapture=true;break;}
  var ref=isClassCapture?SupportReference(capsule):compiled.support&&Object.prototype.hasOwnProperty.call(compiled.support,caps[i])?SupportReference(compiled.support[caps[i]]):EnvironmentReference(caps[i],env,EnvironmentGlobalReference(caps[i]));
  Push(up,EvalCapture(ref));
 }
 var row=compiled.row;
 return row[2](row[0],row[1],[],up,compiled.program.capStart,compiled.program.pcount,callerThis,true,undefined,target,env);
}
function SupportReference(value){return {get:function(){return value;},set:function(){throw TypeError('Immutable runtime binding');},type:function(){return typeof value;},del:function(){return false;},receiver:undefined};}
function IndirectEval(source){
 var env=ReferenceOwn(Programs,"globalEnvironment");
 if(!env){env={b:Object.create(null),p:null,g:true,v:true};Object.defineProperty(Programs,"globalEnvironment",{value:env,writable:true,configurable:true});}
 return EvaluateInEnvironment({e:[0],c:1,i:true},env,undefined,Programs.evalIntrinsic,[source],globalThis,undefined);
}
function EvalCapture(reference){return ReferenceDescriptor(reference,(function(){return this===undefined;})());}

"#.to_string()
}

pub(crate) fn handler(op: usize) -> &'static str {
    match op {
        79 => "n=C[pc++];VE=BeginVariableEnvironment(consts[n],VE);break;",
        80 => "n=C[pc++];nextEnvironment=CaptureEnvironment(consts[n],VE);break;",
        81 => {
            "n=C[pc++];a=Pop(S);f=Pop(S);o=Pop(S);Push(S,EvaluateInEnvironment(consts[n],VE,o,f,a,receiver,newTarget));break;"
        }
        82 => "n=C[pc++];v=Pop(S);Push(S,EnvironmentReference(consts[n],VE,v));break;",
        _ => "",
    }
}
