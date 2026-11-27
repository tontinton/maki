pub const HELIX_TO_TEXTMATE: &[(&str, &str)] = &[
    ("comment", "comment, comment punctuation.definition.comment"),
    (
        "comment.line",
        "comment.line, comment.line punctuation.definition.comment",
    ),
    (
        "comment.block",
        "comment.block, comment.block punctuation.definition.comment",
    ),
    (
        "comment.line.documentation",
        "comment.line.documentation, comment.line.documentation punctuation.definition.comment",
    ),
    (
        "comment.block.documentation",
        "comment.block.documentation, comment.block.documentation punctuation.definition.comment",
    ),
    ("string", "string, string punctuation.definition.string"),
    (
        "string.regexp",
        "string.regexp, string.regexp punctuation.definition.string",
    ),
    (
        "string.special",
        "string.special, string.quoted.single punctuation.definition.string, string.quoted.double.raw punctuation.definition.string",
    ),
    ("function", "entity.name.function, variable.function"),
    ("function.builtin", "support.function"),
    (
        "function.call",
        "entity.name.function, variable.function, support.function",
    ),
    (
        "function.macro",
        "entity.name.function.macro, support.macro",
    ),
    (
        "function.method",
        "entity.name.function, meta.function-call",
    ),
    ("constructor", "entity.name.function.constructor"),
    (
        "type",
        "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.union, entity.name.impl, support.type, support.class, meta.generic",
    ),
    ("type.builtin", "support.type, storage.type.primitive"),
    ("type.enum.variant", "entity.name.type.enum"),
    ("tag", "entity.name.tag"),
    ("tag.attribute", "entity.other.attribute-name"),
    ("tag.delimiter", "punctuation.definition.tag"),
    ("variable", "variable.other"),
    ("variable.builtin", "variable.language"),
    ("variable.parameter", "variable.parameter"),
    (
        "variable.other.member",
        "variable.other.member, variable.other.property",
    ),
    (
        "constant",
        "constant, variable.other.constant, entity.name.constant",
    ),
    ("constant.builtin", "constant.language"),
    (
        "constant.builtin.boolean",
        "constant.language.boolean, constant.language",
    ),
    (
        "constant.character.escape",
        "constant.character.escape, constant.character.escaped",
    ),
    (
        "keyword.storage.type",
        "storage.type, keyword.declaration, keyword.declaration.function, keyword.declaration.class, keyword.declaration.struct, keyword.declaration.enum, keyword.declaration.trait, keyword.declaration.impl",
    ),
    ("keyword.storage.modifier", "storage.modifier"),
    (
        "keyword.function",
        "keyword.declaration.function, storage.type.function",
    ),
    (
        "keyword.control.import",
        "keyword.control.import, keyword.other",
    ),
    ("keyword.return", "keyword.control.return, keyword.control"),
    ("keyword.directive", "meta.preprocessor"),
    ("keyword.control.exception", "keyword.control.exception"),
    ("punctuation", "punctuation, punctuation.accessor.dot"),
    (
        "punctuation.special",
        "punctuation.section.embedded, punctuation.section.interpolation, punctuation.separator.namespace, punctuation.accessor",
    ),
    ("label", "entity.name.label, storage.modifier.lifetime"),
    (
        "attribute",
        "entity.other.attribute-name, meta.annotation, variable.annotation, meta.annotation punctuation.definition.annotation, meta.annotation punctuation.section.group",
    ),
    (
        "namespace",
        "entity.name.namespace, entity.name.module, meta.path",
    ),
    (
        "markup.raw",
        "markup.raw, markup.raw.inline, markup.raw.block",
    ),
    ("markup.link.url", "markup.underline.link"),
    ("operator", "keyword.operator"),
];

pub fn helix_to_textmate_scope(key: &str) -> &str {
    for &(helix, tm) in HELIX_TO_TEXTMATE {
        if key == helix {
            return tm;
        }
    }
    key
}
