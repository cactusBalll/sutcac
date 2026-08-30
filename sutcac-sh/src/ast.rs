//! Abstract syntax tree for the shell.

/// A shell command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Simple command: assignments, words, and redirects.
    Simple(SimpleCommand),
    /// Command list connected by `;`, `&&`, or `||`.
    Connection {
        left: Box<Command>,
        op: ListOp,
        right: Box<Command>,
    },
    /// Pipeline of commands.
    Pipeline(Vec<Command>),
    /// Group command: `{ list; }` executed in current shell.
    Group(Vec<Command>),
    /// Subshell command: `( list )` executed in a subshell.
    Subshell(Vec<Command>),
    /// Negated command: `! cmd` inverts the exit status.
    Negation(Box<Command>),
    /// Standalone arithmetic evaluation: `((expr))`.
    ArithEval(String),
    /// C-style for loop: `for ((init; cond; step)); do ... done`.
    ArithFor {
        init: String,
        cond: String,
        step: String,
        body: Vec<Command>,
    },
    /// Break out of a loop, optionally with a nesting level.
    Break(Option<usize>),
    /// Continue to the next iteration, optionally with a nesting level.
    Continue(Option<usize>),
    /// If statement.
    If {
        cond: Box<Command>,
        then_part: Vec<Command>,
        elifs: Vec<(Command, Vec<Command>)>,
        else_part: Vec<Command>,
    },
    /// While loop.
    While {
        cond: Box<Command>,
        body: Vec<Command>,
    },
    /// For loop.
    For {
        var: String,
        words: Vec<Word>,
        body: Vec<Command>,
    },
    /// Case statement.
    Case { word: Word, arms: Vec<CaseArm> },
    /// Function definition: name body.
    FunctionDef { name: String, body: Box<Command> },
}

/// Simple command structure.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SimpleCommand {
    /// Environment assignments (e.g. `VAR=value cmd`).
    pub assignments: Vec<(String, Word)>,
    /// Words: command name followed by arguments.
    pub words: Vec<Word>,
    /// Redirections.
    pub redirects: Vec<Redirect>,
}

/// A word is a raw string as it appeared in the source.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    pub value: String,
}

impl Word {
    pub fn new(s: impl Into<String>) -> Self {
        Self { value: s.into() }
    }
}

/// Redirection descriptor.
#[derive(Debug, Clone, PartialEq)]
pub struct Redirect {
    /// Source file descriptor, if explicitly given (e.g. `2>`).
    pub fd: Option<i32>,
    /// Redirection kind.
    pub kind: RedirectKind,
    /// Target word.
    pub target: Word,
}

/// Kinds of redirection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RedirectKind {
    Read,      // <
    Write,     // >
    Append,    // >>
    ReadWrite, // <>
    DupInput,  // <&
    DupOutput, // >&
    Here,      // <<
}

/// List operators.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ListOp {
    Semi, // ;
    And,  // &&
    Or,   // ||
}

/// A single case arm.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseArm {
    pub patterns: Vec<Word>,
    pub body: Vec<Command>,
}
