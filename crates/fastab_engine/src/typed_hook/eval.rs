//! Runtime walker for typed-hook expressions, including T2.2 locals and
//! control flow.  String operations stay on the parent UTF-16 helpers so the
//! T2.1 goldens keep one implementation.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::rc::Rc;

use serde::Deserialize;
use serde_json::Value as JsonValue;

use super::{
    TypedExecRequest, TypedExpr, TypedHookEffects, TypedHookError, TypedHookResult, TypedObjectField, TypedValue,
    Utf16String, ensure_safe_integer, ensure_string_limit, js_at, js_char_at, js_map_case, js_pad, js_repeat,
    js_replace, js_slice, js_slice_range, js_split, js_substring, js_trim, safe_arithmetic, typed_value_to_json,
    utf16_ends_with, utf16_index_of, utf16_index_of_i64, utf16_last_index_of_i64, utf16_starts_with,
};

enum Abort {
    Error(TypedHookError),
    Return(TypedValue),
    Break,
    Continue,
}

type EvalResult<T> = Result<T, Abort>;

impl From<TypedHookError> for Abort {
    fn from(error: TypedHookError) -> Self {
        Self::Error(error)
    }
}

pub(super) fn evaluate(expression: &TypedExpr, arguments: &[TypedValue]) -> TypedHookResult<TypedValue> {
    evaluate_with_effects(expression, arguments, None)
}

pub(super) fn evaluate_with_effects(
    expression: &TypedExpr,
    arguments: &[TypedValue],
    effects: Option<&TypedHookEffects<'_>>,
) -> TypedHookResult<TypedValue> {
    let _regex_scope = RegexCacheScope::enter();
    let mut locals = BTreeMap::new();
    let result = evaluate_inner(expression, arguments, &mut locals, effects);
    crate::cancellation::check().map_err(|_| TypedHookError::cancelled())?;
    match result {
        Ok(value) => Ok(value),
        Err(Abort::Return(value)) => Ok(value),
        Err(Abort::Break) => Err(TypedHookError::new("break outside loop")),
        Err(Abort::Continue) => Err(TypedHookError::new("continue outside loop")),
        Err(Abort::Error(error)) => Err(error),
    }
}

fn fail(message: impl Into<String>) -> Abort {
    Abort::Error(TypedHookError::new(message))
}

/// The hook's wall-clock budget, checked inside the two loop forms. QuickJS
/// enforced this from an interrupt handler on any JS; typed IR only has to
/// guard the places that can run unbounded. Checked every `DEADLINE_STRIDE`
/// iterations so a hot loop does not pay for a clock read per step.
const DEADLINE_STRIDE: u32 = 1024;

fn loop_deadline_expired(effects: Option<&TypedHookEffects<'_>>, iterations: u32) -> bool {
    if !iterations.is_multiple_of(DEADLINE_STRIDE) {
        return false;
    }
    let Some(deadline) = effects.and_then(|effects| effects.deadline) else {
        return false;
    };
    deadline.checked_duration_since(std::time::Instant::now()).is_none()
}

fn loop_timed_out(op: &str) -> Abort {
    Abort::Error(TypedHookError::timed_out(format!(
        "typed hook {op} exceeded its deadline"
    )))
}

fn evaluate_inner(
    expression: &TypedExpr,
    arguments: &[TypedValue],
    locals: &mut BTreeMap<String, TypedValue>,
    effects: Option<&TypedHookEffects<'_>>,
) -> EvalResult<TypedValue> {
    crate::cancellation::check().map_err(|_| TypedHookError::cancelled())?;
    let ev = |expression: &TypedExpr, locals: &mut BTreeMap<String, TypedValue>| {
        evaluate_inner(expression, arguments, locals, effects)
    };
    match expression {
        TypedExpr::Arg { index } => {
            let index = usize::try_from(*index).map_err(|error| fail(format!("argument index {index}: {error}")))?;
            arguments
                .get(index)
                .cloned()
                .ok_or_else(|| fail(format!("argument index {index} is unavailable")))
        },
        TypedExpr::String { value } => Ok(TypedValue::String(Utf16String::from_str(value))),
        TypedExpr::Bool { value } => Ok(TypedValue::Bool(*value)),
        TypedExpr::Null => Ok(TypedValue::Null),
        TypedExpr::Integer { value } => {
            ensure_safe_integer(*value, "integer")?;
            Ok(TypedValue::Integer(*value))
        },
        TypedExpr::Array { items } => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(ev(item, locals)?);
            }
            Ok(TypedValue::Array(values))
        },
        TypedExpr::Length { value } => Ok(TypedValue::Integer(safe_len(&ev(value, locals)?)?)),
        TypedExpr::StringIncludes { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "string-includes.value")?;
            let needle = into_utf16(ev(needle, locals)?, "string-includes.needle")?;
            Ok(TypedValue::Bool(utf16_index_of(&value, &needle).is_some()))
        },
        TypedExpr::StringIndexOf { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "string-index-of.value")?;
            let needle = into_utf16(ev(needle, locals)?, "string-index-of.needle")?;
            Ok(TypedValue::Integer(utf16_index_of_i64(&value, &needle)?))
        },
        TypedExpr::StringIndexOfFrom { value, needle, start } => {
            let value = into_utf16(ev(value, locals)?, "string-index-of-from.value")?;
            let needle = into_utf16(ev(needle, locals)?, "string-index-of-from.needle")?;
            let start = as_i64(&ev(start, locals)?, "string-index-of-from.start")?;
            Ok(TypedValue::Integer(utf16_index_of_from(&value, &needle, start)?))
        },
        TypedExpr::StringLastIndexOf { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "string-last-index-of.value")?;
            let needle = into_utf16(ev(needle, locals)?, "string-last-index-of.needle")?;
            Ok(TypedValue::Integer(utf16_last_index_of_i64(&value, &needle)?))
        },
        TypedExpr::StringSlice { value, start } => {
            let value = into_utf16(ev(value, locals)?, "string-slice.value")?;
            let start = as_i64(&ev(start, locals)?, "string-slice.start")?;
            Ok(TypedValue::String(js_slice(&value, start)?))
        },
        TypedExpr::StringSliceRange { value, start, end } => {
            let Some(value) = receiver_utf16(ev(value, locals)?, "string-slice-range.value")? else {
                return Ok(TypedValue::Null);
            };
            let start = as_i64(&ev(start, locals)?, "string-slice-range.start")?;
            let end = as_i64(&ev(end, locals)?, "string-slice-range.end")?;
            Ok(TypedValue::String(js_slice_range(&value, start, end)?))
        },
        TypedExpr::StringSliceAfterFirst { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "string-slice-after-first.value")?;
            let needle = into_utf16(ev(needle, locals)?, "string-slice-after-first.needle")?;
            let Some(index) = utf16_index_of(&value, &needle) else {
                return Ok(TypedValue::String(value));
            };
            let start = i64::try_from(index + 1).map_err(|error| fail(format!("slice start: {error}")))?;
            Ok(TypedValue::String(js_slice(&value, start)?))
        },
        TypedExpr::StringSubstring { value, start, end } => {
            let value = into_utf16(ev(value, locals)?, "string-substring.value")?;
            let start = as_i64(&ev(start, locals)?, "string-substring.start")?;
            let end = as_i64(&ev(end, locals)?, "string-substring.end")?;
            Ok(TypedValue::String(js_substring(&value, start, end)?))
        },
        TypedExpr::StringSplit { value, separator } => {
            let value = into_utf16(ev(value, locals)?, "string-split.value")?;
            let separator = into_utf16(ev(separator, locals)?, "string-split.separator")?;
            Ok(TypedValue::StringArray(js_split(&value, &separator)))
        },
        TypedExpr::StringSplitLimit {
            value,
            separator,
            limit,
        } => {
            let value = into_utf16(ev(value, locals)?, "string-split-limit.value")?;
            let separator = into_utf16(ev(separator, locals)?, "string-split-limit.separator")?;
            let limit = as_i64(&ev(limit, locals)?, "string-split-limit.limit")?;
            Ok(TypedValue::StringArray(js_split_limit(&value, &separator, limit)))
        },
        TypedExpr::StringTrim { value } => {
            let Some(value) = receiver_utf16(ev(value, locals)?, "trim")? else {
                return Ok(TypedValue::Null);
            };
            Ok(TypedValue::String(js_trim(&value, true, true)))
        },
        TypedExpr::StringTrimStart { value } => {
            let Some(value) = receiver_utf16(ev(value, locals)?, "trim-start")? else {
                return Ok(TypedValue::Null);
            };
            Ok(TypedValue::String(js_trim(&value, true, false)))
        },
        TypedExpr::StringTrimEnd { value } => {
            let Some(value) = receiver_utf16(ev(value, locals)?, "trim-end")? else {
                return Ok(TypedValue::Null);
            };
            Ok(TypedValue::String(js_trim(&value, false, true)))
        },
        TypedExpr::StringReplace {
            value,
            needle,
            replacement,
        } => {
            let value = into_utf16(ev(value, locals)?, "replace.value")?;
            let needle = into_utf16(ev(needle, locals)?, "replace.needle")?;
            let replacement = into_utf16(ev(replacement, locals)?, "replace.replacement")?;
            Ok(TypedValue::String(js_replace(&value, &needle, &replacement, false)?))
        },
        TypedExpr::StringReplaceAll {
            value,
            needle,
            replacement,
        } => {
            let value = into_utf16(ev(value, locals)?, "replace-all.value")?;
            let needle = into_utf16(ev(needle, locals)?, "replace-all.needle")?;
            let replacement = into_utf16(ev(replacement, locals)?, "replace-all.replacement")?;
            Ok(TypedValue::String(js_replace(&value, &needle, &replacement, true)?))
        },
        TypedExpr::StringStartsWith { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "starts-with.value")?;
            let needle = into_utf16(ev(needle, locals)?, "starts-with.needle")?;
            Ok(TypedValue::Bool(utf16_starts_with(&value, &needle)))
        },
        TypedExpr::StringEndsWith { value, needle } => {
            let value = into_utf16(ev(value, locals)?, "ends-with.value")?;
            let needle = into_utf16(ev(needle, locals)?, "ends-with.needle")?;
            Ok(TypedValue::Bool(utf16_ends_with(&value, &needle)))
        },
        TypedExpr::StringToLower { value } => Ok(TypedValue::String(js_map_case(
            &into_utf16(ev(value, locals)?, "to-lower")?,
            false,
        ))),
        TypedExpr::StringToUpper { value } => Ok(TypedValue::String(js_map_case(
            &into_utf16(ev(value, locals)?, "to-upper")?,
            true,
        ))),
        TypedExpr::StringPadStart { value, target, pad } => {
            let value = into_utf16(ev(value, locals)?, "pad-start.value")?;
            let target = as_i64(&ev(target, locals)?, "pad-start.target")?;
            let pad = into_utf16(ev(pad, locals)?, "pad-start.pad")?;
            Ok(TypedValue::String(js_pad(&value, target, &pad, false)?))
        },
        TypedExpr::StringPadEnd { value, target, pad } => {
            let value = into_utf16(ev(value, locals)?, "pad-end.value")?;
            let target = as_i64(&ev(target, locals)?, "pad-end.target")?;
            let pad = into_utf16(ev(pad, locals)?, "pad-end.pad")?;
            Ok(TypedValue::String(js_pad(&value, target, &pad, true)?))
        },
        TypedExpr::StringRepeat { value, count } => {
            let value = into_utf16(ev(value, locals)?, "repeat.value")?;
            let count = as_i64(&ev(count, locals)?, "repeat.count")?;
            Ok(TypedValue::String(js_repeat(&value, count)?))
        },
        TypedExpr::StringConcat { parts } => {
            let mut units = Vec::new();
            for part in parts {
                units.extend_from_slice(&into_utf16(ev(part, locals)?, "concat")?.0);
            }
            Ok(TypedValue::String(ensure_string_limit(units)?))
        },
        TypedExpr::StringCharAt { value, index } => {
            let value = into_utf16(ev(value, locals)?, "char-at.value")?;
            let index = as_i64(&ev(index, locals)?, "char-at.index")?;
            Ok(TypedValue::String(js_char_at(&value, index)))
        },
        TypedExpr::StringAt { value, index } => {
            let value = into_utf16(ev(value, locals)?, "at.value")?;
            let index = as_i64(&ev(index, locals)?, "at.index")?;
            Ok(TypedValue::String(js_at(&value, index)?))
        },
        TypedExpr::ArrayIncludes { value, needle } => {
            let value = ev(value, locals)?;
            let needle = ev(needle, locals)?;
            Ok(TypedValue::Bool(array_includes(&value, &needle)))
        },
        TypedExpr::StrictEq { left, right } => {
            Ok(TypedValue::Bool(strict_equal(&ev(left, locals)?, &ev(right, locals)?)))
        },
        TypedExpr::StrictNe { left, right } => {
            Ok(TypedValue::Bool(!strict_equal(&ev(left, locals)?, &ev(right, locals)?)))
        },
        TypedExpr::Add { left, right } => Ok(TypedValue::Integer(safe_arithmetic(
            as_i64(&ev(left, locals)?, "add.left")?.checked_add(as_i64(&ev(right, locals)?, "add.right")?),
            "add",
        )?)),
        TypedExpr::Sub { left, right } => Ok(TypedValue::Integer(safe_arithmetic(
            as_i64(&ev(left, locals)?, "sub.left")?.checked_sub(as_i64(&ev(right, locals)?, "sub.right")?),
            "sub",
        )?)),
        TypedExpr::Mul { left, right } => Ok(TypedValue::Integer(safe_arithmetic(
            as_i64(&ev(left, locals)?, "mul.left")?.checked_mul(as_i64(&ev(right, locals)?, "mul.right")?),
            "mul",
        )?)),
        TypedExpr::LessThan { left, right } => Ok(TypedValue::Bool(
            as_i64(&ev(left, locals)?, "lt.left")? < as_i64(&ev(right, locals)?, "lt.right")?,
        )),
        TypedExpr::LessThanOrEqual { left, right } => Ok(TypedValue::Bool(
            as_i64(&ev(left, locals)?, "le.left")? <= as_i64(&ev(right, locals)?, "le.right")?,
        )),
        TypedExpr::GreaterThan { left, right } => Ok(TypedValue::Bool(
            as_i64(&ev(left, locals)?, "gt.left")? > as_i64(&ev(right, locals)?, "gt.right")?,
        )),
        TypedExpr::GreaterThanOrEqual { left, right } => Ok(TypedValue::Bool(
            as_i64(&ev(left, locals)?, "ge.left")? >= as_i64(&ev(right, locals)?, "ge.right")?,
        )),
        TypedExpr::Not { value } => Ok(TypedValue::Bool(!is_truthy(&ev(value, locals)?))),
        TypedExpr::Nullish { left, right } => {
            let left = ev(left, locals)?;
            if matches!(left, TypedValue::Null) {
                ev(right, locals)
            } else {
                Ok(left)
            }
        },
        TypedExpr::And { left, right } => {
            let left = ev(left, locals)?;
            if !is_truthy(&left) { Ok(left) } else { ev(right, locals) }
        },
        TypedExpr::Or { left, right } => {
            let left = ev(left, locals)?;
            if is_truthy(&left) { Ok(left) } else { ev(right, locals) }
        },
        TypedExpr::If {
            condition,
            then_branch,
            else_branch,
        } => {
            if is_truthy(&ev(condition, locals)?) {
                ev(then_branch, locals)
            } else {
                ev(else_branch, locals)
            }
        },
        TypedExpr::Lambda { .. } => Ok(TypedValue::Null),
        TypedExpr::Var { name } => locals
            .get(name)
            .cloned()
            .ok_or_else(|| fail(format!("unbound variable {name}"))),
        TypedExpr::Let { name, value, body } => {
            let value = ev(value, locals)?;
            let previous = locals.insert(name.clone(), value);
            let result = ev(body, locals);
            match previous {
                Some(previous) => {
                    locals.insert(name.clone(), previous);
                },
                None => {
                    locals.remove(name);
                },
            }
            result
        },
        TypedExpr::Block { items } | TypedExpr::Seq { items } => {
            let mut last = TypedValue::Null;
            for item in items {
                last = ev(item, locals)?;
            }
            Ok(last)
        },
        TypedExpr::Return { value } => Err(Abort::Return(ev(value, locals)?)),
        TypedExpr::CatchReturn { body } => match ev(body, locals) {
            Err(Abort::Return(value)) => Ok(value),
            other => other,
        },
        TypedExpr::Break => Err(Abort::Break),
        TypedExpr::Continue => Err(Abort::Continue),
        TypedExpr::Try { body, catch } => match ev(body, locals) {
            Ok(value) => Ok(value),
            Err(Abort::Return(value)) => Err(Abort::Return(value)),
            Err(Abort::Break) => Err(Abort::Break),
            Err(Abort::Continue) => Err(Abort::Continue),
            Err(Abort::Error(error)) if error.is_cancelled() => Err(Abort::Error(error)),
            Err(Abort::Error(_)) => ev(catch, locals),
        },
        TypedExpr::ForOf { names, value, body } => {
            let items = into_array(ev(value, locals)?);
            let mut iterations = 0_u32;
            for item in items {
                iterations = iterations.saturating_add(1);
                if loop_deadline_expired(effects, iterations) {
                    return Err(loop_timed_out("for-of"));
                }
                bind_for_of(locals, names, item);
                match ev(body, locals) {
                    Ok(_) | Err(Abort::Continue) => {},
                    Err(Abort::Break) => break,
                    Err(other) => return Err(other),
                }
            }
            Ok(TypedValue::Null)
        },
        TypedExpr::AssignVar { name, value } => {
            let value = ev(value, locals)?;
            locals.insert(name.clone(), value.clone());
            Ok(value)
        },
        TypedExpr::AssignProp { object, key, value } => {
            let key = into_utf16(ev(key, locals)?, "assign-prop.key")?.to_string_lossy();
            let value = ev(value, locals)?;
            if let TypedExpr::Var { name } = object.as_ref() {
                let mut current = locals.get(name).cloned().unwrap_or(TypedValue::Null);
                set_prop(&mut current, &key, value.clone());
                locals.insert(name.clone(), current);
            }
            Ok(value)
        },
        TypedExpr::Truthy { value } => Ok(TypedValue::Bool(is_truthy(&ev(value, locals)?))),
        TypedExpr::LooseEq { left, right } => {
            Ok(TypedValue::Bool(loose_equal(&ev(left, locals)?, &ev(right, locals)?)))
        },
        TypedExpr::LooseNe { left, right } => {
            Ok(TypedValue::Bool(!loose_equal(&ev(left, locals)?, &ev(right, locals)?)))
        },
        TypedExpr::Typeof { value } => Ok(TypedValue::String(Utf16String::from_str(js_typeof(&ev(
            value, locals,
        )?)))),
        TypedExpr::Object { fields } => Ok(TypedValue::Object(eval_fields(
            fields, arguments, locals, None, effects,
        )?)),
        TypedExpr::JsonObject { fields } => Ok(TypedValue::Object(eval_fields(
            fields, arguments, locals, None, effects,
        )?)),
        TypedExpr::Spread { value, fields } => {
            let base = ev(value, locals)?;
            Ok(TypedValue::Object(eval_fields(
                fields,
                arguments,
                locals,
                Some(base),
                effects,
            )?))
        },
        TypedExpr::ObjectAssign { parts } => {
            let mut fields = Vec::new();
            for part in parts {
                object_assign(&mut fields, object_map(ev(part, locals)?));
            }
            Ok(TypedValue::Object(fields))
        },
        TypedExpr::Get { value, key } | TypedExpr::JsonGet { value, key } => {
            let value = ev(value, locals)?;
            let key = into_utf16(ev(key, locals)?, "get.key")?.to_string_lossy();
            Ok(get_prop(&value, &key))
        },
        TypedExpr::ObjectKeys { value } => Ok(TypedValue::Array(
            object_keys(&ev(value, locals)?)
                .into_iter()
                .map(|key| TypedValue::String(Utf16String::from_str(&key)))
                .collect(),
        )),
        TypedExpr::ObjectEntries { value } => Ok(TypedValue::Array(
            object_entries(&ev(value, locals)?)
                .into_iter()
                .map(|(key, child)| TypedValue::Array(vec![TypedValue::String(Utf16String::from_str(&key)), child]))
                .collect(),
        )),
        TypedExpr::ObjectValues { value } => Ok(TypedValue::Array(
            object_entries(&ev(value, locals)?)
                .into_iter()
                .map(|(_, child)| child)
                .collect(),
        )),
        TypedExpr::ArrayMap { value, callback } => {
            let items = into_array(ev(value, locals)?);
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.into_iter().enumerate() {
                out.push(apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item, TypedValue::Integer(index as i64)],
                    effects,
                )?);
            }
            Ok(TypedValue::Array(out))
        },
        TypedExpr::ArrayFilter { value, callback } => {
            let items = into_array(ev(value, locals)?);
            let mut out = Vec::new();
            for (index, item) in items.into_iter().enumerate() {
                if is_truthy(&apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item.clone(), TypedValue::Integer(index as i64)],
                    effects,
                )?) {
                    out.push(item);
                }
            }
            Ok(TypedValue::Array(out))
        },
        TypedExpr::ArrayFlatMap { value, callback } => {
            let items = into_array(ev(value, locals)?);
            let mut out = Vec::new();
            for (index, item) in items.into_iter().enumerate() {
                let mapped = apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item, TypedValue::Integer(index as i64)],
                    effects,
                )?;
                out.extend(into_array(mapped));
            }
            Ok(TypedValue::Array(out))
        },
        TypedExpr::ArraySlice { value, start, end } => {
            let items = into_array(ev(value, locals)?);
            let start = as_i64(&ev(start, locals)?, "array-slice.start")?;
            let end = as_i64(&ev(end, locals)?, "array-slice.end")?;
            Ok(TypedValue::Array(slice_vec(&items, start, end)))
        },
        TypedExpr::ArrayJoin { value, separator } => {
            let items = into_array(ev(value, locals)?);
            let separator = into_utf16(ev(separator, locals)?, "array-join.separator")?.to_string_lossy();
            let joined = items
                .iter()
                .map(|item| as_utf16(item, "array-join.item").map(|value| value.to_string_lossy()))
                .collect::<Result<Vec<_>, _>>()?
                .join(&separator);
            Ok(TypedValue::String(Utf16String::from_str(&joined)))
        },
        TypedExpr::ArraySome { value, callback } => {
            let items = into_array(ev(value, locals)?);
            for (index, item) in items.into_iter().enumerate() {
                if is_truthy(&apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item, TypedValue::Integer(index as i64)],
                    effects,
                )?) {
                    return Ok(TypedValue::Bool(true));
                }
            }
            Ok(TypedValue::Bool(false))
        },
        TypedExpr::ArrayEvery { value, callback } => {
            let items = into_array(ev(value, locals)?);
            for (index, item) in items.into_iter().enumerate() {
                if !is_truthy(&apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item, TypedValue::Integer(index as i64)],
                    effects,
                )?) {
                    return Ok(TypedValue::Bool(false));
                }
            }
            Ok(TypedValue::Bool(true))
        },
        TypedExpr::ArrayFind { value, callback } => {
            let items = into_array(ev(value, locals)?);
            for (index, item) in items.into_iter().enumerate() {
                if is_truthy(&apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item.clone(), TypedValue::Integer(index as i64)],
                    effects,
                )?) {
                    return Ok(item);
                }
            }
            Ok(TypedValue::Null)
        },
        TypedExpr::ArrayFindIndex { value, callback } => {
            let items = into_array(ev(value, locals)?);
            for (index, item) in items.into_iter().enumerate() {
                if is_truthy(&apply_lambda(
                    callback,
                    arguments,
                    locals,
                    &[item, TypedValue::Integer(index as i64)],
                    effects,
                )?) {
                    return Ok(TypedValue::Integer(index as i64));
                }
            }
            Ok(TypedValue::Integer(-1))
        },
        TypedExpr::ArrayIndexOf { value, needle } => {
            let items = into_array(ev(value, locals)?);
            let needle = ev(needle, locals)?;
            let index = items.iter().position(|item| strict_equal(item, &needle));
            Ok(TypedValue::Integer(index.map(|index| index as i64).unwrap_or(-1)))
        },
        TypedExpr::ArrayIndexOfFrom { value, needle, start } => {
            let items = into_array(ev(value, locals)?);
            let needle = ev(needle, locals)?;
            let start = js_index(as_i64(&ev(start, locals)?, "array-index-of-from.start")?, items.len());
            let index = items[start..]
                .iter()
                .position(|item| strict_equal(item, &needle))
                .map(|index| (index + start) as i64);
            Ok(TypedValue::Integer(index.unwrap_or(-1)))
        },
        TypedExpr::ArrayConcat { parts } => {
            let mut out = Vec::new();
            for part in parts {
                let value = ev(part, locals)?;
                if matches!(
                    value,
                    TypedValue::Array(_)
                        | TypedValue::StringArray(_)
                        | TypedValue::StringSet(_)
                        | TypedValue::Json(JsonValue::Array(_))
                ) {
                    out.extend(into_array(value));
                } else {
                    out.push(value);
                }
            }
            Ok(TypedValue::Array(out))
        },
        TypedExpr::ArrayReverse { value } => {
            let mut items = into_array(ev(value, locals)?);
            items.reverse();
            Ok(TypedValue::Array(items))
        },
        TypedExpr::ArraySort { value, callback } => {
            let mut items = into_array(ev(value, locals)?);
            items.sort_by(|left, right| {
                match apply_lambda(callback, arguments, locals, &[left.clone(), right.clone()], effects) {
                    Ok(TypedValue::Integer(value)) => value.cmp(&0),
                    _ => std::cmp::Ordering::Equal,
                }
            });
            Ok(TypedValue::Array(items))
        },
        TypedExpr::ArrayIndex { value, index } => {
            let value = ev(value, locals)?;
            let index = ev(index, locals)?;
            if is_array_value(&value) {
                if let Ok(index) = as_i64(&index, "array-index") {
                    if index < 0 {
                        return Ok(TypedValue::Null);
                    }
                    return Ok(into_array(value)
                        .into_iter()
                        .nth(index as usize)
                        .unwrap_or(TypedValue::Null));
                }
            }
            let key = into_utf16(index, "array-index")?.to_string_lossy();
            Ok(get_prop(&value, &key))
        },
        TypedExpr::ArrayPush { name, item } => {
            let item = ev(item, locals)?;
            // Take the binding instead of copying it: a push loop must not
            // clone the whole array once per element.
            let mut items = locals.remove(name).map(into_array).unwrap_or_default();
            items.push(item);
            let length = items.len() as i64;
            locals.insert(name.clone(), TypedValue::Array(items));
            Ok(TypedValue::Integer(length))
        },
        TypedExpr::ArrayPop { value } => {
            let mut items = into_array(ev(value, locals)?);
            Ok(items.pop().unwrap_or(TypedValue::Null))
        },
        TypedExpr::ArrayEntries { value } => Ok(TypedValue::Array(
            into_array(ev(value, locals)?)
                .into_iter()
                .enumerate()
                .map(|(index, item)| TypedValue::Array(vec![TypedValue::Integer(index as i64), item]))
                .collect(),
        )),
        TypedExpr::ArrayShift { name } => {
            let mut items = locals.remove(name).map(into_array).unwrap_or_default();
            let first = if items.is_empty() {
                TypedValue::Null
            } else {
                items.remove(0)
            };
            locals.insert(name.clone(), TypedValue::Array(items));
            Ok(first)
        },
        TypedExpr::ArrayFrom { value } => {
            let value = ev(value, locals)?;
            if let Some(length) = object_length(&value) {
                let count = as_i64(&length, "array-from.length")?.max(0);
                let count = usize::try_from(count).unwrap_or(0).min(10_000);
                return Ok(TypedValue::Array(vec![TypedValue::Null; count]));
            }
            Ok(TypedValue::Array(into_array(value)))
        },
        TypedExpr::ArrayFlat { value, depth } => {
            let depth = as_i64(&ev(depth, locals)?, "array-flat.depth")?;
            Ok(TypedValue::Array(flatten(into_array(ev(value, locals)?), depth)))
        },
        TypedExpr::JsonParse { value } => {
            let text = into_utf16(ev(value, locals)?, "json-parse")?.to_string_lossy();
            Ok(parse_js_json(&text).unwrap_or(TypedValue::Null))
        },
        TypedExpr::JsonArrayItems { value } => Ok(TypedValue::Array(into_array(ev(value, locals)?))),
        TypedExpr::JsonAsString { value } => match ev(value, locals)? {
            TypedValue::String(value) => Ok(TypedValue::String(value)),
            _ => Ok(TypedValue::Null),
        },
        TypedExpr::JsonAsNumber { value } => Ok(to_js_number(&ev(value, locals)?)),
        TypedExpr::JsonAsBool { value } => match ev(value, locals)? {
            TypedValue::Bool(value) => Ok(TypedValue::Bool(value)),
            _ => Ok(TypedValue::Null),
        },
        TypedExpr::JsonStringify { value } => {
            let json = typed_value_to_json(&ev(value, locals)?)?;
            Ok(TypedValue::String(Utf16String::from_str(&json.to_string())))
        },
        TypedExpr::RegexTest { value, pattern, flags } => {
            let value = into_utf16(ev(value, locals)?, "regex-test")?.to_string_lossy();
            Ok(TypedValue::Bool(regex_is_match(pattern, flags, &value)?))
        },
        TypedExpr::RegexMatch { value, pattern, flags } => {
            let value = into_utf16(ev(value, locals)?, "regex-match")?.to_string_lossy();
            Ok(regex_match(pattern, flags, &value)?)
        },
        TypedExpr::RegexMatchAll { value, pattern, flags } => {
            let value = into_utf16(ev(value, locals)?, "regex-match-all")?.to_string_lossy();
            Ok(regex_match_all(pattern, flags, &value)?)
        },
        TypedExpr::RegexReplace {
            value,
            pattern,
            flags,
            replacement,
        } => {
            let value = into_utf16(ev(value, locals)?, "regex-replace.value")?.to_string_lossy();
            let replacement = into_utf16(ev(replacement, locals)?, "regex-replace.replacement")?.to_string_lossy();
            Ok(TypedValue::String(Utf16String::from_str(&regex_replace(
                pattern,
                flags,
                &value,
                &replacement,
            )?)))
        },
        TypedExpr::RegexSearch { value, pattern, flags } => {
            let value = into_utf16(ev(value, locals)?, "regex-search")?.to_string_lossy();
            Ok(TypedValue::Integer(regex_search(pattern, flags, &value)?))
        },
        TypedExpr::StringSplitRegex { value, pattern, flags } => {
            let value = into_utf16(ev(value, locals)?, "string-split-regex")?.to_string_lossy();
            Ok(TypedValue::Array(
                regex_split(pattern, flags, &value)?
                    .into_iter()
                    .map(|part| TypedValue::String(Utf16String::from_str(&part)))
                    .collect(),
            ))
        },
        TypedExpr::MathMax { values } => {
            let items = into_array(ev(values, locals)?);
            let mut max = i64::MIN;
            for item in items {
                max = max.max(as_i64(&item, "math-max")?);
            }
            Ok(TypedValue::Integer(max))
        },
        TypedExpr::StringSet { value } => {
            let mut set = Vec::new();
            for item in into_array(ev(value, locals)?) {
                string_set_insert(&mut set, into_utf16(item, "string-set")?);
            }
            Ok(TypedValue::StringSet(set))
        },
        TypedExpr::StringSetAdd { name, item } => {
            let item = into_utf16(ev(item, locals)?, "string-set-add")?;
            let mut set = match locals.get(name) {
                Some(TypedValue::StringSet(set)) => set.clone(),
                Some(other) => {
                    let mut set = Vec::new();
                    for item in as_array(other) {
                        if let Ok(text) = as_utf16(&item, "string-set") {
                            string_set_insert(&mut set, text);
                        }
                    }
                    set
                },
                None => Vec::new(),
            };
            string_set_insert(&mut set, item);
            locals.insert(name.clone(), TypedValue::StringSet(set.clone()));
            Ok(TypedValue::StringSet(set))
        },
        TypedExpr::Regex { pattern, flags } => Ok(TypedValue::Regex {
            pattern: pattern.clone(),
            flags: flags.clone(),
        }),
        TypedExpr::While { condition, body } => {
            let mut guard = 0_u32;
            loop {
                if !is_truthy(&ev(condition, locals)?) {
                    break;
                }
                guard += 1;
                if loop_deadline_expired(effects, guard) {
                    return Err(loop_timed_out("while loop"));
                }
                if guard > 100_000 {
                    // The runaway guard stands in for the wall clock on a loop
                    // whose body is too cheap to reach the deadline, so it
                    // reports the same outcome a budget overrun does.
                    return Err(Abort::Error(TypedHookError::timed_out(
                        "while loop exceeded iteration cap",
                    )));
                }
                match ev(body, locals) {
                    Ok(_) | Err(Abort::Continue) => {},
                    Err(Abort::Break) => break,
                    Err(other) => return Err(other),
                }
            }
            Ok(TypedValue::Null)
        },
        TypedExpr::ToString { value } => Ok(TypedValue::String(into_utf16(ev(value, locals)?, "to-string")?)),
        TypedExpr::LocaleCompare { left, right } => {
            let left = into_utf16(ev(left, locals)?, "locale-compare.left")?.to_string_lossy();
            let right = into_utf16(ev(right, locals)?, "locale-compare.right")?.to_string_lossy();
            Ok(TypedValue::Integer(js_locale_compare(&left, &right)))
        },
        TypedExpr::Exec {
            command,
            args,
            cwd,
            env,
            timeout,
        } => evaluate_exec(
            &ev(command, locals)?,
            &ev(args, locals)?,
            &ev(cwd, locals)?,
            &ev(env, locals)?,
            &ev(timeout, locals)?,
            effects,
        ),
        TypedExpr::Par { items } => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(ev(item, locals)?);
            }
            Ok(TypedValue::Array(values))
        },
        TypedExpr::CtxCwd => Ok(TypedValue::String(Utf16String::from_str(
            &require_effects(effects)?.context.current_working_directory,
        ))),
        TypedExpr::CtxProcess => Ok(TypedValue::String(Utf16String::from_str(
            &require_effects(effects)?.context.current_process,
        ))),
        TypedExpr::CtxSshPrefix => Ok(TypedValue::String(Utf16String::from_str(
            &require_effects(effects)?.context.ssh_prefix,
        ))),
        TypedExpr::CtxSearchTerm => Ok(TypedValue::String(Utf16String::from_str(
            &require_effects(effects)?.context.search_term,
        ))),
        TypedExpr::CtxIsDangerous => Ok(TypedValue::Bool(require_effects(effects)?.context.is_dangerous)),
        TypedExpr::CtxEnvironment => Ok(TypedValue::Object(
            require_effects(effects)?
                .context
                .environment_variables
                .iter()
                .map(|(key, value)| (key.clone(), TypedValue::String(Utf16String::from_str(value))))
                .collect(),
        )),
        TypedExpr::CtxEnv { name } => {
            let name = into_utf16(ev(name, locals)?, "ctx-env.name")?.to_string_lossy();
            let value = require_effects(effects)?
                .context
                .environment_variables
                .get(&name)
                .map(String::as_str);
            Ok(match value {
                Some(value) => TypedValue::String(Utf16String::from_str(value)),
                None => TypedValue::Null,
            })
        },
        TypedExpr::SpecObject { fields } => Ok(TypedValue::Object(eval_fields(
            fields, arguments, locals, None, effects,
        )?)),
        TypedExpr::Throw { class, message } => {
            let message = into_utf16(ev(message, locals)?, "throw.message")?.to_string_lossy();
            Err(Abort::Error(TypedHookError::throw(class, message)))
        },
    }
}

fn require_effects<'a, 'e>(effects: Option<&'e TypedHookEffects<'a>>) -> EvalResult<&'e TypedHookEffects<'a>> {
    effects.ok_or_else(|| fail("effect operation requires typed hook effects"))
}

fn evaluate_exec(
    command: &TypedValue,
    args: &TypedValue,
    cwd: &TypedValue,
    env: &TypedValue,
    timeout: &TypedValue,
    effects: Option<&TypedHookEffects<'_>>,
) -> EvalResult<TypedValue> {
    let effects = require_effects(effects)?;
    if let Some(deadline) = effects.deadline {
        if deadline.checked_duration_since(std::time::Instant::now()).is_none() {
            return Err(Abort::Error(TypedHookError::timed_out(
                "typed hook exec exceeded its deadline",
            )));
        }
    }
    let command = as_utf16(command, "exec.command")?.to_string_lossy();
    let args = as_array(args)
        .into_iter()
        .map(|item| {
            into_utf16(item, "exec.args")
                .map(|value| value.to_string_lossy())
                .map_err(Abort::from)
        })
        .collect::<EvalResult<Vec<_>>>()?;
    let cwd = match cwd {
        TypedValue::Null => None,
        other => Some(as_utf16(other, "exec.cwd")?.to_string_lossy()),
    };
    let env = match env {
        TypedValue::Null => Vec::new(),
        other => object_entries(other)
            .into_iter()
            .map(|(key, value)| {
                into_utf16(value, "exec.env")
                    .map(|text| (key, text.to_string_lossy()))
                    .map_err(Abort::from)
            })
            .collect::<EvalResult<Vec<_>>>()?,
    };
    let timeout_ms = match timeout {
        TypedValue::Null => None,
        other => Some(as_i64(other, "exec.timeout")? as u64),
    };
    let mut request = TypedExecRequest {
        command,
        args,
        cwd,
        env,
        timeout_ms,
    };
    if let Some(deadline) = effects.deadline {
        if let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            let remaining_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
            request.timeout_ms = Some(
                request
                    .timeout_ms
                    .map_or(remaining_ms, |timeout| timeout.min(remaining_ms)),
            );
        }
    }
    crate::cancellation::check().map_err(|_| TypedHookError::cancelled())?;
    let result = (effects.exec)(request);
    crate::cancellation::check().map_err(|_| TypedHookError::cancelled())?;
    let result = result?;
    Ok(TypedValue::Object(vec![
        (
            "stdout".into(),
            TypedValue::String(Utf16String::from_str(&result.stdout)),
        ),
        (
            "stderr".into(),
            TypedValue::String(Utf16String::from_str(&result.stderr)),
        ),
        ("status".into(), TypedValue::Integer(result.status)),
    ]))
}

fn apply_lambda(
    callback: &TypedExpr,
    arguments: &[TypedValue],
    locals: &mut BTreeMap<String, TypedValue>,
    values: &[TypedValue],
    effects: Option<&TypedHookEffects<'_>>,
) -> EvalResult<TypedValue> {
    let TypedExpr::Lambda { params, body } = callback else {
        return Err(fail("array callback is not a lambda"));
    };
    let saved: Vec<(String, Option<TypedValue>)> = params
        .iter()
        .map(|name| (name.clone(), locals.get(name).cloned()))
        .collect();
    for (index, name) in params.iter().enumerate() {
        locals.insert(name.clone(), values.get(index).cloned().unwrap_or(TypedValue::Null));
    }
    let result = match evaluate_inner(body, arguments, locals, effects) {
        Err(Abort::Return(value)) => Ok(value),
        other => other,
    };
    for (name, previous) in saved {
        match previous {
            Some(previous) => {
                locals.insert(name, previous);
            },
            None => {
                locals.remove(&name);
            },
        }
    }
    result
}

fn eval_fields(
    fields: &[TypedObjectField],
    arguments: &[TypedValue],
    locals: &mut BTreeMap<String, TypedValue>,
    base: Option<TypedValue>,
    effects: Option<&TypedHookEffects<'_>>,
) -> EvalResult<Vec<(String, TypedValue)>> {
    let mut object = match base {
        Some(value) => object_map(value),
        None => Vec::new(),
    };
    for field in fields {
        object_insert(
            &mut object,
            field.key.clone(),
            evaluate_inner(&field.value, arguments, locals, effects)?,
        );
    }
    Ok(object)
}

fn bind_for_of(locals: &mut BTreeMap<String, TypedValue>, names: &[String], item: TypedValue) {
    if names.len() == 1 {
        locals.insert(names[0].clone(), item);
        return;
    }
    let mut items = into_array(item).into_iter();
    for name in names {
        locals.insert(name.clone(), items.next().unwrap_or(TypedValue::Null));
    }
}

fn receiver_utf16(value: TypedValue, label: &str) -> EvalResult<Option<Utf16String>> {
    if matches!(value, TypedValue::Null) {
        return Ok(None);
    }
    Ok(Some(into_utf16(value, label)?))
}

/// [`as_utf16`] for a value the caller no longer needs: a string moves out
/// instead of being copied.
fn into_utf16(value: TypedValue, label: &str) -> TypedHookResult<Utf16String> {
    match value {
        TypedValue::String(value) => Ok(value),
        other => as_utf16(&other, label),
    }
}

fn as_utf16(value: &TypedValue, label: &str) -> TypedHookResult<Utf16String> {
    match value {
        TypedValue::String(value) => Ok(value.clone()),
        TypedValue::Null => Ok(Utf16String::from_str("undefined")),
        TypedValue::Bool(value) => Ok(Utf16String::from_str(if *value { "true" } else { "false" })),
        TypedValue::Integer(value) => Ok(Utf16String::from_str(&value.to_string())),
        TypedValue::Json(JsonValue::String(value)) => Ok(Utf16String::from_str(value)),
        TypedValue::Json(JsonValue::Null) => Ok(Utf16String::from_str("null")),
        other => typed_value_to_json(other)
            .map(|json| match json {
                JsonValue::String(value) => Utf16String::from_str(&value),
                JsonValue::Null => Utf16String::from_str(""),
                other => Utf16String::from_str(&other.to_string()),
            })
            .map_err(|_| TypedHookError::new(format!("{label} is not string-like"))),
    }
}

fn as_i64(value: &TypedValue, label: &str) -> TypedHookResult<i64> {
    match value {
        TypedValue::Integer(value) => Ok(*value),
        TypedValue::Bool(true) => Ok(1),
        TypedValue::Bool(false) | TypedValue::Null => Ok(0),
        TypedValue::String(value) => value
            .to_string_lossy()
            .parse::<f64>()
            .ok()
            .and_then(|number| (number.is_finite() && number == number.trunc()).then_some(number as i64))
            .ok_or_else(|| TypedHookError::new(format!("{label} is not an integer"))),
        TypedValue::Json(JsonValue::Number(number)) => number
            .as_i64()
            .ok_or_else(|| TypedHookError::new(format!("{label} is not an integer"))),
        _ => Err(TypedHookError::new(format!("{label} is not an integer"))),
    }
}

fn is_array_value(value: &TypedValue) -> bool {
    matches!(
        value,
        TypedValue::Array(_)
            | TypedValue::StringArray(_)
            | TypedValue::StringSet(_)
            | TypedValue::Json(JsonValue::Array(_))
    )
}

/// Approximate Node's default `localeCompare` for CLI tokens: punctuation
/// and symbols sort before letters, then code points.  That is what the
/// example/trigger file-list helper relies on (`{{{` before `not json`).
fn js_locale_compare(left: &str, right: &str) -> i64 {
    let mut left_chars = left.chars();
    let mut right_chars = right.chars();
    loop {
        match (left_chars.next(), right_chars.next()) {
            (None, None) => return 0,
            (None, Some(_)) => return -1,
            (Some(_), None) => return 1,
            (Some(left), Some(right)) => {
                let left_rank = locale_rank(left);
                let right_rank = locale_rank(right);
                if left_rank != right_rank {
                    return i64::from(left_rank > right_rank) * 2 - 1;
                }
            },
        }
    }
}

fn locale_rank(ch: char) -> (u8, char) {
    let class = if ch.is_ascii_alphanumeric() { 2 } else { 1 };
    (class, ch)
}

fn string_set_insert(set: &mut Vec<Utf16String>, item: Utf16String) {
    if !set.iter().any(|existing| existing == &item) {
        set.push(item);
    }
}

/// [`as_array`] for a value the caller no longer needs: array storage moves
/// out instead of every element being copied.
fn into_array(value: TypedValue) -> Vec<TypedValue> {
    match value {
        TypedValue::Array(items) => items,
        TypedValue::StringArray(items) | TypedValue::StringSet(items) => {
            items.into_iter().map(TypedValue::String).collect()
        },
        TypedValue::Json(JsonValue::Array(items)) => items.iter().map(json_to_value).collect(),
        TypedValue::Null => Vec::new(),
        other => vec![other],
    }
}

fn as_array(value: &TypedValue) -> Vec<TypedValue> {
    match value {
        TypedValue::Array(items) => items.clone(),
        TypedValue::StringArray(items) => items.iter().cloned().map(TypedValue::String).collect(),
        TypedValue::StringSet(items) => items.iter().cloned().map(TypedValue::String).collect(),
        TypedValue::Json(JsonValue::Array(items)) => items.iter().map(json_to_value).collect(),
        TypedValue::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// `JSON.parse` that keeps object key insertion order.  `serde_json::Value`
/// maps are `BTreeMap` unless the crate-wide `preserve_order` feature is on,
/// which would scramble `Object.entries` on package.json `scripts`.
fn parse_js_json(text: &str) -> Result<TypedValue, serde_json::Error> {
    serde_json::from_str::<JsJson>(text).map(|value| value.0)
}

struct JsJson(TypedValue);

impl<'de> Deserialize<'de> for JsJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsJsonVisitor)
    }
}

struct JsJsonVisitor;

impl<'de> serde::de::Visitor<'de> for JsJsonVisitor {
    type Value = JsJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::Bool(value)))
    }

    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::Integer(value)))
    }

    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        i64::try_from(value)
            .map(TypedValue::Integer)
            .map(JsJson)
            .map_err(|_| serde::de::Error::custom("JSON number is outside the safe integer range"))
    }

    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::Json(JsonValue::from(value))))
    }

    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::String(Utf16String::from_str(value))))
    }

    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::String(Utf16String::from_str(&value))))
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::Null))
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(JsJson(TypedValue::Null))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        while let Some(JsJson(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(JsJson(TypedValue::Array(items)))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut fields = Vec::new();
        while let Some((key, JsJson(child))) = map.next_entry()? {
            object_insert(&mut fields, key, child);
        }
        Ok(JsJson(TypedValue::Object(fields)))
    }
}

fn json_to_value(value: &JsonValue) -> TypedValue {
    match value {
        JsonValue::Null => TypedValue::Null,
        JsonValue::Bool(value) => TypedValue::Bool(*value),
        JsonValue::Number(number) => number
            .as_i64()
            .map(TypedValue::Integer)
            .unwrap_or_else(|| TypedValue::Json(value.clone())),
        JsonValue::String(value) => TypedValue::String(Utf16String::from_str(value)),
        JsonValue::Array(items) => TypedValue::Array(items.iter().map(json_to_value).collect()),
        JsonValue::Object(fields) => TypedValue::Object(
            fields
                .iter()
                .map(|(key, child)| (key.clone(), json_to_value(child)))
                .collect(),
        ),
    }
}

fn is_truthy(value: &TypedValue) -> bool {
    match value {
        TypedValue::Null => false,
        TypedValue::Bool(value) => *value,
        TypedValue::Integer(0) => false,
        TypedValue::Integer(_) => true,
        TypedValue::String(value) => !value.0.is_empty(),
        TypedValue::StringArray(items) => !items.is_empty(),
        TypedValue::Array(items) => !items.is_empty(),
        TypedValue::Object(fields) => !fields.is_empty(),
        TypedValue::StringSet(items) => !items.is_empty(),
        TypedValue::Regex { .. } => true,
        TypedValue::Json(value) => match value {
            JsonValue::Null => false,
            JsonValue::Bool(value) => *value,
            JsonValue::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
            JsonValue::String(value) => !value.is_empty(),
            JsonValue::Array(items) => !items.is_empty(),
            JsonValue::Object(fields) => !fields.is_empty(),
        },
    }
}

fn strict_equal(left: &TypedValue, right: &TypedValue) -> bool {
    match (left, right) {
        (TypedValue::Null, TypedValue::Null) => true,
        (TypedValue::Bool(left), TypedValue::Bool(right)) => left == right,
        (TypedValue::Integer(left), TypedValue::Integer(right)) => left == right,
        (TypedValue::String(left), TypedValue::String(right)) => left == right,
        (TypedValue::String(left), TypedValue::Json(JsonValue::String(right))) => left.to_string_lossy() == *right,
        (TypedValue::Json(JsonValue::String(left)), TypedValue::String(right)) => *left == right.to_string_lossy(),
        (TypedValue::Json(left), TypedValue::Json(right)) => left == right,
        _ => false,
    }
}

fn loose_equal(left: &TypedValue, right: &TypedValue) -> bool {
    if strict_equal(left, right) {
        return true;
    }
    match (as_i64(left, "loose").ok(), as_i64(right, "loose").ok()) {
        (Some(left), Some(right)) => left == right,
        _ => as_utf16(left, "loose")
            .ok()
            .zip(as_utf16(right, "loose").ok())
            .is_some_and(|(left, right)| left == right),
    }
}

fn js_typeof(value: &TypedValue) -> &'static str {
    match value {
        TypedValue::Null => "object",
        TypedValue::Bool(_) => "boolean",
        TypedValue::Integer(_) => "number",
        TypedValue::String(_) => "string",
        TypedValue::Regex { .. } => "object",
        _ => "object",
    }
}

fn get_prop(value: &TypedValue, key: &str) -> TypedValue {
    match value {
        TypedValue::Object(fields) => object_get(fields, key).cloned().unwrap_or(TypedValue::Null),
        TypedValue::Json(JsonValue::Object(fields)) => fields.get(key).map(json_to_value).unwrap_or(TypedValue::Null),
        TypedValue::Array(items) if key.parse::<usize>().is_ok() => items
            .get(key.parse::<usize>().unwrap_or(usize::MAX))
            .cloned()
            .unwrap_or(TypedValue::Null),
        TypedValue::String(value) if key == "length" => TypedValue::Integer(value.len() as i64),
        TypedValue::Array(items) if key == "length" => TypedValue::Integer(items.len() as i64),
        _ => TypedValue::Null,
    }
}

fn set_prop(value: &mut TypedValue, key: &str, child: TypedValue) {
    match value {
        TypedValue::Object(fields) => {
            object_insert(fields, key.to_string(), child);
        },
        TypedValue::Json(JsonValue::Object(fields)) => {
            if let Ok(json) = typed_value_to_json(&child) {
                fields.insert(key.to_string(), json);
            }
        },
        other => {
            let mut fields = object_map(other.clone());
            object_insert(&mut fields, key.to_string(), child);
            *other = TypedValue::Object(fields);
        },
    }
}

fn object_get<'a>(fields: &'a [(String, TypedValue)], key: &str) -> Option<&'a TypedValue> {
    fields
        .iter()
        .rev()
        .find(|(name, _)| name == key)
        .map(|(_, child)| child)
}

fn object_insert(fields: &mut Vec<(String, TypedValue)>, key: String, child: TypedValue) {
    if let Some((_, existing)) = fields.iter_mut().find(|(name, _)| *name == key) {
        *existing = child;
        return;
    }
    fields.push((key, child));
}

fn object_assign(target: &mut Vec<(String, TypedValue)>, source: Vec<(String, TypedValue)>) {
    for (key, child) in source {
        object_insert(target, key, child);
    }
}

fn object_map(value: TypedValue) -> Vec<(String, TypedValue)> {
    match value {
        TypedValue::Object(fields) => fields,
        TypedValue::Json(JsonValue::Object(fields)) => fields
            .into_iter()
            .map(|(key, child)| (key, json_to_value(&child)))
            .collect(),
        _ => Vec::new(),
    }
}

fn object_length(value: &TypedValue) -> Option<TypedValue> {
    match value {
        TypedValue::Object(fields) => object_get(fields, "length").cloned(),
        TypedValue::Json(JsonValue::Object(fields)) => fields.get("length").map(json_to_value),
        _ => None,
    }
}

fn to_js_number(value: &TypedValue) -> TypedValue {
    if let TypedValue::String(text) = value {
        if text.0.is_empty() {
            return TypedValue::Integer(0);
        }
    }
    as_i64(value, "number").map_or(TypedValue::Null, TypedValue::Integer)
}

fn object_keys(value: &TypedValue) -> Vec<String> {
    match value {
        TypedValue::Object(fields) => fields.iter().map(|(key, _)| key.clone()).collect(),
        TypedValue::Json(JsonValue::Object(fields)) => fields.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn object_entries(value: &TypedValue) -> Vec<(String, TypedValue)> {
    match value {
        TypedValue::Object(fields) => fields.clone(),
        TypedValue::Json(JsonValue::Object(fields)) => fields
            .iter()
            .map(|(key, child)| (key.clone(), json_to_value(child)))
            .collect(),
        _ => Vec::new(),
    }
}

fn array_includes(value: &TypedValue, needle: &TypedValue) -> bool {
    if let TypedValue::StringSet(set) = value {
        if let Ok(needle) = as_utf16(needle, "includes") {
            return set.contains(&needle);
        }
    }
    match value {
        TypedValue::Array(items) => items.iter().any(|item| strict_equal(item, needle)),
        other => as_array(other).iter().any(|item| strict_equal(item, needle)),
    }
}

fn safe_len(value: &TypedValue) -> TypedHookResult<i64> {
    let length = match value {
        TypedValue::String(value) => value.len(),
        TypedValue::StringArray(items) => items.len(),
        TypedValue::Array(items) => items.len(),
        TypedValue::StringSet(items) => items.len(),
        TypedValue::Object(fields) => fields.len(),
        TypedValue::Json(JsonValue::String(value)) => value.encode_utf16().count(),
        TypedValue::Json(JsonValue::Array(items)) => items.len(),
        TypedValue::Json(JsonValue::Object(fields)) => fields.len(),
        TypedValue::Null => 0,
        _ => 0,
    };
    let length = i64::try_from(length).map_err(|error| TypedHookError::new(format!("length is too large: {error}")))?;
    ensure_safe_integer(length, "length")?;
    Ok(length)
}

fn slice_vec(items: &[TypedValue], start: i64, end: i64) -> Vec<TypedValue> {
    let len = items.len() as i64;
    let start = js_index(start, items.len()) as i64;
    let end = if end < 0 { (len + end).max(0) } else { end.min(len) };
    if start >= end {
        return Vec::new();
    }
    items[start as usize..end as usize].to_vec()
}

fn js_index(index: i64, len: usize) -> usize {
    let len = len as i64;
    let actual = if index < 0 {
        (len + index).max(0)
    } else {
        index.min(len)
    };
    actual as usize
}

fn flatten(items: Vec<TypedValue>, depth: i64) -> Vec<TypedValue> {
    if depth <= 0 {
        return items;
    }
    let mut out = Vec::new();
    for item in items {
        if matches!(item, TypedValue::Array(_) | TypedValue::StringArray(_)) {
            out.extend(flatten(into_array(item), depth - 1));
        } else {
            out.push(item);
        }
    }
    out
}

fn utf16_index_of_from(value: &Utf16String, needle: &Utf16String, start: i64) -> TypedHookResult<i64> {
    let start = js_index(start, value.len());
    if needle.0.is_empty() {
        return Ok(start as i64);
    }
    if start > value.len() || needle.0.len() > value.0.len().saturating_sub(start) {
        return Ok(-1);
    }
    Ok(value.0[start..]
        .windows(needle.0.len())
        .position(|window| window == needle.0.as_slice())
        .map(|index| (index + start) as i64)
        .unwrap_or(-1))
}

fn js_split_limit(value: &Utf16String, separator: &Utf16String, limit: i64) -> Vec<Utf16String> {
    if limit <= 0 {
        return Vec::new();
    }
    let mut parts = js_split(value, separator);
    parts.truncate(limit as usize);
    parts
}

thread_local! {
    /// Regex literals compiled by the evaluation running on this thread. An
    /// array callback evaluates the same literal once per element; compile it
    /// once. The cache lives only as long as the outermost evaluation, so no
    /// compiled program stays resident between hooks.
    static REGEX_CACHE: RefCell<RegexCache> = RefCell::new(RegexCache::default());
}

#[derive(Default)]
struct RegexCache {
    depth: usize,
    compiled: HashMap<String, Rc<fancy_regex::Regex>>,
}

struct RegexCacheScope;

impl RegexCacheScope {
    fn enter() -> Self {
        REGEX_CACHE.with_borrow_mut(|cache| cache.depth += 1);
        Self
    }
}

impl Drop for RegexCacheScope {
    fn drop(&mut self) {
        REGEX_CACHE.with_borrow_mut(|cache| {
            cache.depth -= 1;
            if cache.depth == 0 {
                cache.compiled = HashMap::new();
            }
        });
    }
}

fn compile_regex(pattern: &str, flags: &str) -> TypedHookResult<Rc<fancy_regex::Regex>> {
    let mut builder = String::new();
    if flags.contains('i') {
        builder.push_str("(?i)");
    }
    if flags.contains('m') {
        builder.push_str("(?m)");
    }
    if flags.contains('s') {
        builder.push_str("(?s)");
    }
    builder.push_str(pattern);
    let cached = REGEX_CACHE.with_borrow(|cache| cache.compiled.get(&builder).cloned());
    if let Some(regex) = cached {
        return Ok(regex);
    }
    // Only successes are kept: an invalid literal fails the same way each time.
    let regex = Rc::new(
        fancy_regex::Regex::new(&builder).map_err(|error| TypedHookError::new(format!("regex compile: {error}")))?,
    );
    REGEX_CACHE.with_borrow_mut(|cache| {
        if cache.depth > 0 {
            cache.compiled.insert(builder, Rc::clone(&regex));
        }
    });
    Ok(regex)
}

fn regex_is_match(pattern: &str, flags: &str, value: &str) -> TypedHookResult<bool> {
    compile_regex(pattern, flags)?
        .is_match(value)
        .map_err(|error| TypedHookError::new(format!("regex test: {error}")))
}

fn regex_search(pattern: &str, flags: &str, value: &str) -> TypedHookResult<i64> {
    match compile_regex(pattern, flags)?.find(value) {
        Ok(Some(found)) => Ok(value[..found.start()].encode_utf16().count() as i64),
        Ok(None) => Ok(-1),
        Err(error) => Err(TypedHookError::new(format!("regex search: {error}"))),
    }
}

fn regex_match(pattern: &str, flags: &str, value: &str) -> TypedHookResult<TypedValue> {
    if flags.contains('g') {
        let matches = regex_match_all(pattern, flags, value)?;
        return Ok(match &matches {
            TypedValue::Array(items) if items.is_empty() => TypedValue::Null,
            _ => matches,
        });
    }
    match compile_regex(pattern, flags)?.captures(value) {
        Ok(Some(captures)) => {
            let mut items = Vec::new();
            for index in 0..captures.len() {
                items.push(TypedValue::String(Utf16String::from_str(
                    captures.get(index).map(|part| part.as_str()).unwrap_or(""),
                )));
            }
            Ok(TypedValue::Array(items))
        },
        Ok(None) => Ok(TypedValue::Null),
        Err(error) => Err(TypedHookError::new(format!("regex match: {error}"))),
    }
}

fn regex_match_all(pattern: &str, flags: &str, value: &str) -> TypedHookResult<TypedValue> {
    let regex = compile_regex(pattern, flags)?;
    let mut matches = Vec::new();
    for capture in regex.captures_iter(value) {
        let capture = capture.map_err(|error| TypedHookError::new(format!("regex matchAll: {error}")))?;
        let mut items = Vec::new();
        for index in 0..capture.len() {
            items.push(TypedValue::String(Utf16String::from_str(
                capture.get(index).map(|part| part.as_str()).unwrap_or(""),
            )));
        }
        matches.push(TypedValue::Array(items));
    }
    if flags.contains('g') && !pattern.contains('(') {
        // JS String.prototype.match with /g/ returns the full-match strings, not capture arrays.
        let flat = matches
            .into_iter()
            .filter_map(|item| match item {
                TypedValue::Array(mut items) => items.drain(..1).next(),
                other => Some(other),
            })
            .collect();
        return Ok(TypedValue::Array(flat));
    }
    Ok(TypedValue::Array(matches))
}

fn regex_replace(pattern: &str, flags: &str, value: &str, replacement: &str) -> TypedHookResult<String> {
    let regex = compile_regex(pattern, flags)?;
    if flags.contains('g') {
        Ok(regex.replace_all(value, replacement).into_owned())
    } else {
        Ok(regex.replace(value, replacement).into_owned())
    }
}

fn regex_split(pattern: &str, flags: &str, value: &str) -> TypedHookResult<Vec<String>> {
    let regex = compile_regex(pattern, flags)?;
    Ok(regex.split(value).filter_map(Result::ok).map(str::to_string).collect())
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use crate::typed_hook::{TypedExecResult, TypedHookContext};

    fn exec_expression() -> TypedExpr {
        TypedExpr::Exec {
            command: Box::new(TypedExpr::String {
                value: "test-command".into(),
            }),
            args: Box::new(TypedExpr::Array { items: Vec::new() }),
            cwd: Box::new(TypedExpr::Null),
            env: Box::new(TypedExpr::Null),
            timeout: Box::new(TypedExpr::Null),
        }
    }

    fn context() -> TypedHookContext {
        TypedHookContext {
            current_working_directory: String::new(),
            current_process: String::new(),
            ssh_prefix: String::new(),
            environment_variables: Default::default(),
            search_term: String::new(),
            is_dangerous: false,
        }
    }

    #[test]
    fn typed_try_cannot_catch_a_cancelled_exec() {
        let calls = std::cell::Cell::new(0);
        let exec = |_| {
            calls.set(calls.get() + 1);
            Err(TypedHookError::cancelled())
        };
        let context = context();
        let effects = TypedHookEffects {
            context: &context,
            exec: &exec,
            deadline: None,
        };
        let expression = TypedExpr::Try {
            body: Box::new(exec_expression()),
            catch: Box::new(exec_expression()),
        };
        let error = evaluate_with_effects(&expression, &[], Some(&effects)).unwrap_err();
        assert!(error.is_cancelled());
        assert!(!error.is_timed_out());
        assert_eq!(calls.get(), 1, "the catch body must never run");
    }

    #[test]
    fn typed_loop_observes_cancellation_after_an_effect_handshake() {
        let token = crate::cancellation::CancellationToken::new();
        let _scope = crate::cancellation::enter(token.clone());
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let canceller = std::thread::spawn(move || {
            started_rx.recv().unwrap();
            token.cancel();
        });
        let exec = |_| {
            started_tx.send(()).unwrap();
            Ok(TypedExecResult {
                stdout: String::new(),
                stderr: String::new(),
                status: 0,
            })
        };
        let context = context();
        let effects = TypedHookEffects {
            context: &context,
            exec: &exec,
            deadline: Some(std::time::Instant::now() + std::time::Duration::from_secs(5)),
        };
        let expression = TypedExpr::Par {
            items: vec![
                exec_expression(),
                TypedExpr::While {
                    condition: Box::new(TypedExpr::Bool { value: true }),
                    body: Box::new(TypedExpr::Null),
                },
            ],
        };
        let error = evaluate_with_effects(&expression, &[], Some(&effects)).unwrap_err();
        canceller.join().unwrap();
        assert!(error.is_cancelled());
        assert!(!error.is_timed_out());
    }

    #[test]
    fn regex_literals_compile_once_per_evaluation_and_are_released_after() {
        {
            let _scope = RegexCacheScope::enter();
            let first = compile_regex("a+", "i").unwrap();
            let second = compile_regex("a+", "i").unwrap();
            assert!(Rc::ptr_eq(&first, &second));
            assert!(compile_regex("(", "").is_err());
        }
        REGEX_CACHE.with_borrow(|cache| {
            assert_eq!(cache.depth, 0);
            assert!(cache.compiled.is_empty());
        });
        let outside = compile_regex("a+", "i").unwrap();
        assert!(!Rc::ptr_eq(&outside, &compile_regex("a+", "i").unwrap()));
    }
}
