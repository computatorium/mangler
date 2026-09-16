//! Native lexical reference primitives used by static and dynamic compilation.
use swc_core::ecma::ast::AssignOp;

#[derive(Clone, Copy)]
pub enum Operation {
    Reference,
    Delete,
    Call { optional: bool },
    OptionalReference,
    OptionalCallReference { receiver: bool, value: bool },
    In,
    Update { increment: bool, prefix: bool },
    Assign(AssignOp),
    Construct,
    This,
    NewTarget,
}
impl Operation {
    pub fn id(self) -> u32 {
        match self {
            Self::Reference => 0,
            Self::Delete => 35,
            Self::Call { optional: false } => 1,
            Self::Call { optional: true } => 2,
            Self::OptionalReference => 3,
            Self::OptionalCallReference { receiver, value } => {
                4 + u32::from(receiver) + 2 * u32::from(value)
            }
            Self::In => 8,
            Self::Update { increment, prefix } => {
                9 + 2 * u32::from(!increment) + u32::from(!prefix)
            }
            Self::Assign(op) => {
                16 + ASSIGNMENTS
                    .iter()
                    .position(|candidate| *candidate == op)
                    .expect("all assignment operations") as u32
            }
            Self::Construct => 32,
            Self::This => 33,
            Self::NewTarget => 34,
        }
    }
    pub fn source(self, private: Option<&str>, apply: &str, iterator: &str) -> String {
        let reference = private.map_or_else(|| "super[k]".into(), |key| format!("o.#{key}"));
        let argument = if private.is_some() { "o" } else { "k" };
        match self {
            Self::Delete => "(k)=>delete super[k]".into(),
            Self::Reference if private.is_some() => format!("(o)=>({{get value(){{return {reference}}},set value(v){{{reference}=v}}}})"),
            Self::Reference => "(k)=>{const g=()=>super[k],s=(v)=>super[k]=v;return {get value(){return g()},set value(v){s(v)}}}".into(),
            Self::Call { optional } => {
                let receiver = if private.is_some() { "" } else { ",o=this" };
                let guard = if optional { "if(f===null||f===void 0)return f;" } else { "" };
                format!("({argument})=>{{const f={reference}{receiver};{guard}return (...a)=>{apply}(f,o,a)}}")
            }
            Self::OptionalReference => format!("(o)=>{{if(o===null||o===void 0)return o;return {{get value(){{return {reference}}}}}}}"),
            Self::OptionalCallReference { receiver, value } => {
                let receiver = if receiver { "if(o===null||o===void 0)return o;" } else { "" };
                let value = if value { "if(f===null||f===void 0)return f;" } else { "" };
                format!("(o)=>{{{receiver}return {{get value(){{const f={reference};{value}return (...a)=>{apply}(f,o,a)}}}}}}")
            }
            Self::In => format!("(o)=>#{} in o", private.expect("private brand operation")),
            Self::Update { increment, prefix } => {
                let operator = if increment { "++" } else { "--" };
                let operation = if prefix { format!("{operator}{reference}") } else { format!("{reference}{operator}") };
                format!("({argument})=>{operation}")
            }
            Self::Assign(op) => format!("({argument},r)=>{reference} {} r()", op.as_str()),
            // Source arguments have already been evaluated, including source spreads.
            // The private rest array must not perform another user-observable
            // Array.prototype iterator lookup while initializing derived `this`.
            Self::Construct => format!("(...a)=>super(...{{[{iterator}](){{let i=0;return {{next(){{return i<a.length?{{value:a[i++],done:false}}:{{done:true}}}}}}}}}})"),
            Self::This => "()=>this".into(),
            Self::NewTarget => "()=>new.target".into(),
        }
    }
}
const ASSIGNMENTS: [AssignOp; 16] = [
    AssignOp::Assign,
    AssignOp::AddAssign,
    AssignOp::SubAssign,
    AssignOp::MulAssign,
    AssignOp::DivAssign,
    AssignOp::ModAssign,
    AssignOp::ExpAssign,
    AssignOp::LShiftAssign,
    AssignOp::RShiftAssign,
    AssignOp::ZeroFillRShiftAssign,
    AssignOp::BitAndAssign,
    AssignOp::BitXorAssign,
    AssignOp::BitOrAssign,
    AssignOp::AndAssign,
    AssignOp::OrAssign,
    AssignOp::NullishAssign,
];
fn operations(private: bool) -> Vec<Operation> {
    let mut values = vec![
        Operation::Reference,
        Operation::Call { optional: false },
        Operation::Call { optional: true },
    ];
    if !private {
        values.push(Operation::Delete);
    }
    if private {
        values.extend([Operation::OptionalReference, Operation::In]);
        for receiver in [false, true] {
            for value in [false, true] {
                values.push(Operation::OptionalCallReference { receiver, value });
            }
        }
    }
    for increment in [false, true] {
        for prefix in [false, true] {
            values.push(Operation::Update { increment, prefix });
        }
    }
    values.extend(ASSIGNMENTS.into_iter().map(Operation::Assign));
    values
}
pub fn provider(private: Option<&str>, apply: &str, iterator: &str, construct: bool) -> String {
    let mut operations = operations(private.is_some());
    if construct {
        operations.push(Operation::Construct);
    }
    let cases = operations
        .into_iter()
        .map(|operation| {
            format!(
                "case {}:return {};",
                operation.id(),
                operation.source(private, apply, iterator)
            )
        })
        .collect::<String>();
    format!("(operation)=>{{switch(operation){{{cases}}}}}")
}
