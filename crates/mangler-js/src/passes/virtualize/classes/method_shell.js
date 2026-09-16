var __MCLASS=(function(){
  const define=Object.defineProperty,descriptor=Object.getOwnPropertyDescriptor;
  const getPrototype=Object.getPrototypeOf,setPrototype=Object.setPrototypeOf;
  const ownKeys=Reflect.ownKeys,apply=Reflect.apply,remove=Reflect.deleteProperty;
  const ProxyCtor=Proxy,SymbolCtor=Symbol,WeakMapCtor=WeakMap;
  const weakGet=WeakMapCtor.prototype.get,weakSet=WeakMapCtor.prototype.set;
  const markers=new WeakMapCtor(),wrapped=new WeakMapCtor();
  function lookup(map,key){return apply(weakGet,map,[key])}
  function store(map,key,value){apply(weakSet,map,[key,value]);return value}
  function display(key,accessor){
    const name=({[key](){}})[key].name;
    return accessor===1?'get '+name:accessor===2?'set '+name:name;
  }
  function wrap(target,kind,key,accessor){
    const known=lookup(wrapped,target);
    if(known)return known;
    define(target,'name',{__proto__:null,value:display(key,accessor),writable:false,enumerable:false,configurable:true});
    if(!kind)return target;
    const template=kind===1?async function(){}:kind===2?function*(){}:async function*(){};
    setPrototype(target,getPrototype(template));
    if(kind!==1)define(target,'prototype',{__proto__:null,value:template.prototype,writable:true,enumerable:false,configurable:false});
    const result=new ProxyCtor(target,{__proto__:null,apply(target,receiver,args){
      let value;
      try{value=apply(target,receiver,args)}catch(error){
        if(kind===1)return (async()=>{throw error})();
        throw error;
      }
      if(kind===1)return value;
      const iterator=value;
      const prototype=target.prototype;
      setPrototype(iterator,prototype!==null&&(typeof prototype==='object'||typeof prototype==='function')?prototype:getPrototype(template.prototype));
      return iterator;
    }});
    return store(wrapped,target,result);
  }
  function key(value,kind,accessor,isStatic){
    const property=ownKeys({[value]:0})[0];
    // Native class definition rejects this own nonconfigurable property before
    // evaluating any later computed keys. Keep that operation at key evaluation.
    if(isStatic&&property==='prototype')(class{static [property](){}});
    const marker=SymbolCtor();
    store(markers,marker,[property,kind,accessor]);
    return marker;
  }
  function installOn(object){
    const keys=ownKeys(object);
    for(let i=0;i<keys.length;i++){
      const marker=keys[i],metadata=lookup(markers,marker);
      if(!metadata)continue;
      const property=metadata[0],kind=metadata[1],accessor=metadata[2],entry=descriptor(object,marker);
      const target=accessor===1?entry.get:accessor===2?entry.set:entry.value;
      const callable=wrap(target,kind,property,accessor);
      remove(object,marker);
      // Omit the other accessor field: repeated getters/setters merge in source
      // order just as ClassDefinitionEvaluation defines them.
      if(accessor===1)define(object,property,{__proto__:null,get:callable,enumerable:false,configurable:true});
      else if(accessor===2)define(object,property,{__proto__:null,set:callable,enumerable:false,configurable:true});
      else define(object,property,{__proto__:null,value:callable,writable:true,enumerable:false,configurable:true});
    }
  }
  return {__proto__:null,key,wrap,install(value){installOn(value.prototype);installOn(value)}};
})();
