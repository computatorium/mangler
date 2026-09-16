var tests=0;
function check(value,message){if(!value)throw Error(message);tests++;}
function outcome(fn){try{return fn()}catch(error){return error.name}}
function probe(present=true,remove=false){
 var log=[],target=present?{x:7}:{},object=new Proxy(target,{
  has(t,k){if(k==='x')log.push('has');return Reflect.has(t,k)},
  get(t,k,r){if(k===Symbol.unscopables){log.push('unscopables');if(remove)delete t.x;return undefined}if(k==='x')log.push('get');return Reflect.get(t,k,r)}
 });return {object,log,target};
}
var fallback={get(){throw Error('fallback read')},set(){throw Error('fallback write')},type(){throw Error('fallback type')},del(){return false}};
for(var strict of [false,true]){
 var p=probe(),r=PropertyReference(p.object,'x');
 check(r.get(strict)===7,'present value');check(p.log.join()==='has,get','present HasProperty before Get');
 p=probe(false);r=PropertyReference(p.object,'x');
 check(outcome(()=>r.get(strict))===(strict?'ReferenceError':undefined),'missing binding strict mode');check(p.log.join()==='has','missing binding must skip Get');
 p=probe(true,true);r=ResolveReference(WithReference(fallback,p.object,'x'));
 check(outcome(()=>r.get(strict))===(strict?'ReferenceError':undefined),'deletion during unscopables retains resolved binding');check(p.log.join()==='has,unscopables,has','resolution and read are distinct HasProperty operations');
 p=probe(true,true);r=ResolveReference(WithReference(fallback,p.object,'x'));
 check(outcome(()=>r.type(strict))===(strict?'ReferenceError':'undefined'),'typeof missing resolved binding');check(p.log.join()==='has,unscopables,has','typeof read uses binding check');
 p=probe(true,true);r=WithReference(fallback,p.object,'x');var descriptor=ReferenceDescriptor(r,!strict);
 descriptor=descriptor.get.vmCapture(strict);
 check(outcome(()=>descriptor.get())===(strict?'ReferenceError':undefined),'capture descriptor switches to consumer mode');
 p=probe(true,true);r=WithReference(fallback,p.object,'x');descriptor=EvalCapture(r).get.vmCapture(strict);
 check(outcome(()=>descriptor.get())===(strict?'ReferenceError':undefined),'eval capture switches to compiled consumer mode');
 p=probe(true,true);CaptureReference(0,WithReference(fallback,p.object,'x'));var native=NativeReference(0);
 check(outcome(()=>native[strict?2:0])===(strict?'ReferenceError':undefined),'native accessor chooses consumer mode');
 p=probe(true,true);CaptureReference(0,WithReference(fallback,p.object,'x'));native=NativeReference(0);
 check(outcome(()=>native[strict?3:1])===(strict?'ReferenceError':undefined),'native call accessor chooses consumer mode');
 p=probe();p.target.x=function(){return this===p.object};CaptureReference(0,WithReference(fallback,p.object,'x'));native=NativeReference(0);
 check(native[strict?3:1](),'native call keeps with receiver');
}
var calls=[];var abrupt=new Proxy({x:1},{has(){calls.push('has');throw new RangeError},get(){calls.push('get');return 1}});
check(outcome(()=>PropertyReference(abrupt,'x').get(false))==='RangeError','HasProperty abrupt completion');check(calls.join()==='has','abrupt check skips Get');
for(var strict of [false,true])for(var op of [65,67,70,72]){
 var p=probe(true,true),r=WithReference(fallback,p.object,'x');
 var result=outcome(()=>(strict?strictOpcode:sloppyOpcode)(op,r));
 check(strict?result==='ReferenceError':op===72?Number.isNaN(result):result===(op===70?'undefined':undefined),'opcode '+op+' consumer strictness');
}
Object.defineProperty(L,1,{configurable:true,get:function(){return 17}});
var selfView=NativeReference(1);
check(selfView[0]===17&&selfView[2]===17,'getter-only self reads retain identity in both modes');
selfView[0]=99;check(selfView[0]===17,'sloppy native self write is ignored');
check(outcome(()=>selfView[2]=99)==='TypeError','strict native self write throws');
var selfCapture=ReferenceDescriptor(LocalReference(1,0),false);
check(selfCapture.get()===17,'sloppy self descriptor reads original');
selfCapture=selfCapture.get.vmCapture(true);
check(selfCapture.get()===17,'strict self descriptor reads original');
check(outcome(()=>selfCapture.set(99))==='TypeError','strict rebound self descriptor stays immutable');
globalThis.__out=String(tests);
