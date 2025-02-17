// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::path::PathBuf;
use std::{collections::HashMap, iter::once, rc::Rc};

use i_slint_compiler::parser::{syntax_nodes, SyntaxKind, TextRange};
use i_slint_compiler::{expression_tree, langtype, literals};
use itertools::Itertools;
use lsp_types::Url;
use slint::{Model, SharedString, VecModel};
use slint_interpreter::{DiagnosticLevel, PlatformError};
use smol_str::SmolStr;

use crate::common::{self, ComponentInformation};
use crate::preview::{properties, runtime_properties, SelectionNotification};

#[cfg(target_arch = "wasm32")]
use crate::wasm_prelude::*;

slint::include_modules!();

pub type PropertyDeclarations = HashMap<SmolStr, PropertyDeclaration>;

pub fn create_ui(style: String, experimental: bool) -> Result<PreviewUi, PlatformError> {
    let ui = PreviewUi::new()?;

    // styles:
    let known_styles = once(&"native")
        .chain(i_slint_compiler::fileaccess::styles().iter())
        .filter(|s| s != &&"qt" || i_slint_backend_selector::HAS_NATIVE_STYLE)
        .cloned()
        .sorted()
        .collect::<Vec<_>>();
    let style = if known_styles.contains(&style.as_str()) {
        style
    } else {
        known_styles
            .iter()
            .find(|x| **x == "native")
            .or_else(|| known_styles.first())
            .map(|s| s.to_string())
            .unwrap_or_default()
    };

    let style_model = Rc::new({
        let model = VecModel::default();
        model.extend(known_styles.iter().map(|s| SharedString::from(*s)));
        assert!(model.row_count() > 1);
        model
    });

    let api = ui.global::<Api>();

    api.set_current_style(style.clone().into());
    api.set_experimental(experimental);
    api.set_known_styles(style_model.into());

    api.on_add_new_component(super::add_new_component);
    api.on_rename_component(super::rename_component);
    api.on_style_changed(super::change_style);
    api.on_show_component(super::show_component);
    api.on_show_document(|file, line, column| {
        use lsp_types::{Position, Range};
        let pos = Position::new((line as u32).saturating_sub(1), (column as u32).saturating_sub(1));
        super::ask_editor_to_show_document(&file, Range::new(pos, pos), false)
    });
    api.on_show_document_offset_range(super::show_document_offset_range);
    api.on_show_preview_for(super::show_preview_for);
    api.on_reload_preview(super::reload_preview);
    api.on_unselect(super::element_selection::unselect_element);
    api.on_reselect(super::element_selection::reselect_element);
    api.on_select_at(super::element_selection::select_element_at);
    api.on_selection_stack_at(super::element_selection::selection_stack_at);
    api.on_filter_sort_selection_stack(super::element_selection::filter_sort_selection_stack);
    api.on_find_selected_selection_stack_frame(|stack| {
        stack.iter().find(|frame| frame.is_selected).unwrap_or_default()
    });
    api.on_select_element(|path, offset, x, y| {
        super::element_selection::select_element_at_source_code_position(
            PathBuf::from(path.to_string()),
            crate::preview::TextSize::from(offset as u32),
            Some(i_slint_core::lengths::LogicalPoint::new(x, y)),
            SelectionNotification::Now,
        );
    });
    api.on_select_behind(super::element_selection::select_element_behind);
    api.on_can_drop(super::can_drop_component);
    api.on_drop(super::drop_component);
    api.on_selected_element_resize(super::resize_selected_element);
    api.on_selected_element_can_move_to(super::can_move_selected_element);
    api.on_selected_element_move(super::move_selected_element);
    api.on_selected_element_delete(super::delete_selected_element);

    api.on_test_code_binding(super::test_code_binding);
    api.on_test_string_binding(super::test_string_binding);
    api.on_set_code_binding(super::set_code_binding);
    api.on_set_color_binding(super::set_color_binding);
    api.on_set_string_binding(super::set_string_binding);
    api.on_property_declaration_ranges(super::property_declaration_ranges);

    api.on_set_runtime_property(set_runtime_property);
    api.on_set_runtime_json_properties(set_runtime_json_properties);
    api.on_duplicate_runtime_property_value_table(duplicate_runtime_property_value_table);

    api.on_string_to_color(|s| string_to_color(s.as_ref()).unwrap_or_default());
    api.on_string_is_color(|s| string_to_color(s.as_ref()).is_some());
    api.on_color_to_data(|c| {
        let encoded = c.as_argb_encoded();

        let a = ((encoded & 0xff000000) >> 24) as u8;
        let r = ((encoded & 0x00ff0000) >> 16) as u8;
        let g = ((encoded & 0x0000ff00) >> 8) as u8;
        let b = (encoded & 0x000000ff) as u8;

        ColorData {
            a: a as i32,
            r: r as i32,
            g: g as i32,
            b: b as i32,
            text: format!(
                "#{:08x}",
                ((r as u32) << 24) + ((g as u32) << 16) + ((b as u32) << 8) + (a as u32)
            )
            .into(),
        }
    });
    api.on_rgba_to_color(|r, g, b, a| {
        if (0..256).contains(&r)
            && (0..256).contains(&g)
            && (0..256).contains(&b)
            && (0..256).contains(&a)
        {
            slint::Color::from_argb_u8(a as u8, r as u8, g as u8, b as u8)
        } else {
            slint::Color::default()
        }
    });

    #[cfg(target_vendor = "apple")]
    api.set_control_key_name("command".into());

    #[cfg(target_family = "wasm")]
    if web_sys::window()
        .and_then(|window| window.navigator().platform().ok())
        .map_or(false, |platform| platform.to_ascii_lowercase().contains("mac"))
    {
        api.set_control_key_name("command".into());
    }

    Ok(ui)
}

fn extract_definition_location(ci: &ComponentInformation) -> (SharedString, SharedString) {
    let Some(url) = ci.defined_at.as_ref().map(|da| da.url()) else {
        return (Default::default(), Default::default());
    };

    let path = url.to_file_path().unwrap_or_default();
    let file_name = path.file_name().unwrap_or_default().to_string_lossy().to_string();

    (url.to_string().into(), file_name.into())
}

pub fn ui_set_uses_widgets(ui: &PreviewUi, uses_widgets: bool) {
    let api = ui.global::<Api>();
    api.set_uses_widgets(uses_widgets);
}

pub fn set_diagnostics(ui: &PreviewUi, diagnostics: &[slint_interpreter::Diagnostic]) {
    let summary = diagnostics.iter().fold(DiagnosticSummary::NothingDetected, |acc, d| {
        match (acc, d.level()) {
            (_, DiagnosticLevel::Error) => DiagnosticSummary::Errors,
            (DiagnosticSummary::Errors, DiagnosticLevel::Warning) => DiagnosticSummary::Errors,
            (_, DiagnosticLevel::Warning) => DiagnosticSummary::Warnings,
            // DiagnosticLevel is non-exhaustive:
            (acc, _) => acc,
        }
    });

    let api = ui.global::<Api>();
    api.set_diagnostic_summary(summary);
}

pub fn ui_set_known_components(
    ui: &PreviewUi,
    known_components: &[crate::common::ComponentInformation],
    current_component_index: usize,
) {
    let mut builtins_map: HashMap<String, Vec<ComponentItem>> = Default::default();
    let mut std_widgets_map: HashMap<String, Vec<ComponentItem>> = Default::default();
    let mut path_map: HashMap<PathBuf, (SharedString, Vec<ComponentItem>)> = Default::default();
    let mut library_map: HashMap<String, Vec<ComponentItem>> = Default::default();
    let mut longest_path_prefix = PathBuf::new();

    for (idx, ci) in known_components.iter().enumerate() {
        if ci.is_global {
            continue;
        }
        let (url, pretty_location) = extract_definition_location(ci);
        let item = ComponentItem {
            name: ci.name.clone().into(),
            index: idx.try_into().unwrap(),
            defined_at: url.clone(),
            pretty_location,
            is_user_defined: !(ci.is_builtin || ci.is_std_widget),
            is_currently_shown: idx == current_component_index,
            is_exported: ci.is_exported,
        };

        if let Some(position) = &ci.defined_at {
            if let Some(library) = position.url().path().strip_prefix("/@") {
                library_map.entry(format!("@{library}")).or_default().push(item);
            } else {
                let path = i_slint_compiler::pathutils::clean_path(
                    &(position.url().to_file_path().unwrap_or_default()),
                );
                if path != PathBuf::new() {
                    if longest_path_prefix == PathBuf::new() {
                        longest_path_prefix = path.clone();
                    } else {
                        longest_path_prefix =
                            std::iter::zip(longest_path_prefix.components(), path.components())
                                .take_while(|(l, p)| l == p)
                                .map(|(l, _)| l)
                                .collect();
                    }
                }
                path_map.entry(path).or_insert((url, Vec::new())).1.push(item);
            }
        } else if ci.is_builtin {
            builtins_map.entry(ci.category.clone()).or_default().push(item);
        } else {
            std_widgets_map.entry(ci.category.clone()).or_default().push(item);
        }
    }

    fn sort_subset(mut input: HashMap<String, Vec<ComponentItem>>) -> Vec<ComponentListItem> {
        let mut output = input
            .drain()
            .map(|(k, mut v)| {
                v.sort_by_key(|i| i.name.clone());
                let model = Rc::new(VecModel::from(v));
                ComponentListItem {
                    category: k.into(),
                    file_url: SharedString::new(),
                    components: model.into(),
                }
            })
            .collect::<Vec<_>>();
        output.sort_by_key(|k| k.category.clone());
        output
    }

    let builtin_components = sort_subset(builtins_map);
    let std_widgets_components = sort_subset(std_widgets_map);
    let library_components = sort_subset(library_map);
    let mut file_components = path_map
        .drain()
        .map(|(p, (file_url, mut v))| {
            v.sort_by_key(|i| i.name.clone());
            let model = Rc::new(VecModel::from(v));
            let name = if p == longest_path_prefix {
                p.file_name().unwrap_or_default().to_string_lossy().to_string()
            } else {
                p.strip_prefix(&longest_path_prefix).unwrap_or(&p).to_string_lossy().to_string()
            };
            ComponentListItem { category: name.into(), file_url, components: model.into() }
        })
        .collect::<Vec<_>>();
    file_components.sort_by_key(|k| PathBuf::from(k.category.to_string()));

    let mut all_components = Vec::with_capacity(
        builtin_components.len() + library_components.len() + file_components.len(),
    );
    all_components.extend_from_slice(&builtin_components);
    all_components.extend_from_slice(&std_widgets_components);
    all_components.extend_from_slice(&library_components);
    all_components.extend_from_slice(&file_components);

    let result = Rc::new(VecModel::from(all_components));
    let api = ui.global::<Api>();
    api.set_known_components(result.into());
}

fn to_ui_range(r: TextRange) -> Option<Range> {
    Some(Range {
        start: i32::try_from(u32::from(r.start())).ok()?,
        end: i32::try_from(u32::from(r.end())).ok()?,
    })
}

fn map_property_declaration(
    document_cache: &common::DocumentCache,
    declared_at: &Option<properties::DeclarationInformation>,
    defined_at: PropertyDefinition,
) -> Option<PropertyDeclaration> {
    let da = declared_at.as_ref()?;
    let source_version = document_cache.document_version_by_path(&da.path).unwrap_or(-1);
    let pos = TextRange::new(da.start_position, da.start_position);

    Some(PropertyDeclaration {
        defined_at,
        source_path: da.path.to_string_lossy().to_string().into(),
        source_version,
        range: to_ui_range(pos)?,
    })
}

fn extract_tr_data(tr_node: &syntax_nodes::AtTr, value: &mut PropertyValue) {
    let Some(text) = tr_node
        .child_text(SyntaxKind::StringLiteral)
        .and_then(|s| i_slint_compiler::literals::unescape_string(&s))
    else {
        return;
    };

    let context = tr_node
        .TrContext()
        .and_then(|n| n.child_text(SyntaxKind::StringLiteral))
        .and_then(|s| i_slint_compiler::literals::unescape_string(&s))
        .unwrap_or_default();
    let plural = tr_node
        .TrPlural()
        .and_then(|n| n.child_text(SyntaxKind::StringLiteral))
        .and_then(|s| i_slint_compiler::literals::unescape_string(&s))
        .unwrap_or_default();
    let plural_expression = tr_node
        .TrPlural()
        .and_then(|n| n.child_node(SyntaxKind::Expression))
        .and_then(|e| e.child_node(SyntaxKind::QualifiedName))
        .map(|n| i_slint_compiler::object_tree::QualifiedTypeName::from_node(n.into()))
        .map(|qtn| qtn.to_string());

    // We have expressions -> Edit as code
    if tr_node.Expression().next().is_none() && (plural.is_empty() || plural_expression.is_some()) {
        value.kind = PropertyValueKind::String;
        value.is_translatable = true;
        value.tr_context = context.as_str().into();
        value.tr_plural = plural.as_str().into();
        value.tr_plural_expression = plural_expression.unwrap_or_default().into();
        value.value_string = text.as_str().into();
    }
}

fn convert_number_literal(
    node: &syntax_nodes::Expression,
) -> Option<(f64, i_slint_compiler::expression_tree::Unit)> {
    if let Some(unary) = &node.UnaryOpExpression() {
        let factor = match unary.first_token().unwrap().text() {
            "-" => -1.0,
            "+" => 1.0,
            _ => return None,
        };
        convert_number_literal(&unary.Expression()).map(|(v, u)| (factor * v, u))
    } else {
        let literal = node.child_text(SyntaxKind::NumberLiteral)?;
        let expr = literals::parse_number_literal(literal).ok()?;

        match expr {
            i_slint_compiler::expression_tree::Expression::NumberLiteral(value, unit) => {
                Some((value, unit))
            }
            _ => None,
        }
    }
}

fn extract_value_with_unit_impl(
    expression: &Option<syntax_nodes::Expression>,
    def_val: Option<&expression_tree::Expression>,
    code: &str,
    units: &[i_slint_compiler::expression_tree::Unit],
) -> Option<(PropertyValueKind, f32, i32)> {
    if let Some(expression) = expression {
        if let Some((value, unit)) = convert_number_literal(expression) {
            let index = units.iter().position(|u| u == &unit).or_else(|| {
                (units.is_empty() && unit == i_slint_compiler::expression_tree::Unit::None)
                    .then_some(0_usize)
            })?;

            return Some((PropertyValueKind::Float, value as f32, index as i32));
        }
    } else if code.is_empty() {
        if let Some(expression_tree::Expression::NumberLiteral(value, unit)) = def_val {
            let index = units.iter().position(|u| u == unit).unwrap_or(0);
            return Some((PropertyValueKind::Float, *value as f32, index as i32));
        } else {
            // FIXME: if def_vale is Some but not a NumberLiteral, we should not show "0"
            return Some((PropertyValueKind::Float, 0.0, 0));
        }
    }

    None
}

fn string_to_color(text: &str) -> Option<slint::Color> {
    literals::parse_color_literal(text).map(slint::Color::from_argb_encoded)
}

fn unit_model(units: &[expression_tree::Unit]) -> slint::ModelRc<slint::SharedString> {
    Rc::new(VecModel::from(
        units.iter().map(|u| u.to_string().into()).collect::<Vec<slint::SharedString>>(),
    ))
    .into()
}

fn extract_value_with_unit(
    expression: &Option<syntax_nodes::Expression>,
    def_val: Option<&expression_tree::Expression>,
    units: &[expression_tree::Unit],
    value: &mut PropertyValue,
) {
    let Some((kind, v, index)) =
        extract_value_with_unit_impl(expression, def_val, &value.code, units)
    else {
        return;
    };

    value.kind = kind;
    value.value_float = v;
    value.visual_items = unit_model(units);
    value.value_int = index
}

fn extract_color(
    expression: &syntax_nodes::Expression,
    kind: PropertyValueKind,
    value: &mut PropertyValue,
) -> bool {
    if let Some(text) = expression.child_text(SyntaxKind::ColorLiteral) {
        if let Some(color) = string_to_color(&text) {
            value.kind = kind;
            value.value_brush = slint::Brush::SolidColor(color);
            value.value_string = text.as_str().into();
            return true;
        }
    }
    false
}

fn set_default_brush(
    kind: PropertyValueKind,
    def_val: Option<&expression_tree::Expression>,
    value: &mut PropertyValue,
) {
    use expression_tree::Expression;
    value.kind = kind;
    if let Some(mut def_val) = def_val {
        if let Expression::Cast { from, .. } = def_val {
            def_val = from;
        }
        if let Expression::NumberLiteral(v, _) = def_val {
            value.value_brush = slint::Brush::SolidColor(slint::Color::from_argb_encoded(*v as _));
            return;
        }
    }
    let text = "#00000000";
    let color = literals::parse_color_literal(text).unwrap();
    value.value_string = text.into();
    value.value_brush = slint::Brush::SolidColor(slint::Color::from_argb_encoded(color));
}

fn simplify_value(prop_info: &super::properties::PropertyInformation) -> PropertyValue {
    use i_slint_compiler::expression_tree::Unit;
    use langtype::Type;

    let code_block_or_expression =
        prop_info.defined_at.as_ref().map(|da| da.code_block_or_expression.clone());
    let expression = code_block_or_expression.as_ref().and_then(|cbe| cbe.expression());

    let mut value = PropertyValue {
        code: code_block_or_expression
            .as_ref()
            .map(|cbe| cbe.text().to_string())
            .unwrap_or_default()
            .into(),
        kind: PropertyValueKind::Code,
        ..Default::default()
    };

    let def_val = prop_info.default_value.as_ref();

    match &prop_info.ty {
        Type::Float32 => extract_value_with_unit(&expression, def_val, &[], &mut value),
        Type::Duration => {
            extract_value_with_unit(&expression, def_val, &[Unit::S, Unit::Ms], &mut value)
        }
        Type::PhysicalLength | Type::LogicalLength | Type::Rem => extract_value_with_unit(
            &expression,
            def_val,
            &[Unit::Px, Unit::Cm, Unit::Mm, Unit::In, Unit::Pt, Unit::Phx, Unit::Rem],
            &mut value,
        ),
        Type::Angle => extract_value_with_unit(
            &expression,
            def_val,
            &[Unit::Deg, Unit::Grad, Unit::Turn, Unit::Rad],
            &mut value,
        ),
        Type::Percent => {
            extract_value_with_unit(&expression, def_val, &[Unit::Percent], &mut value)
        }
        Type::Int32 => {
            if let Some(expression) = expression {
                if let Some((v, unit)) = convert_number_literal(&expression) {
                    if unit == i_slint_compiler::expression_tree::Unit::None {
                        value.kind = PropertyValueKind::Integer;
                        value.value_int = v as i32;
                    }
                }
            } else if value.code.is_empty() {
                value.kind = PropertyValueKind::Integer;
            }
        }
        Type::Color => {
            if let Some(expression) = expression {
                extract_color(&expression, PropertyValueKind::Color, &mut value);
                // TODO: Extract `Foo.bar` as Palette `Foo`, entry `bar`.
                // This makes no sense right now, as we have no way to get any
                // information on the palettes.
            } else if value.code.is_empty() {
                set_default_brush(PropertyValueKind::Color, def_val, &mut value);
            }
        }
        Type::Brush => {
            if let Some(expression) = expression {
                extract_color(&expression, PropertyValueKind::Brush, &mut value);
                // TODO: Handle gradients...
            } else if value.code.is_empty() {
                set_default_brush(PropertyValueKind::Brush, def_val, &mut value);
            }
        }
        Type::Bool => {
            if let Some(expression) = expression {
                let qualified_name =
                    expression.QualifiedName().map(|qn| qn.text().to_string()).unwrap_or_default();
                if ["true", "false"].contains(&qualified_name.as_str()) {
                    value.kind = PropertyValueKind::Boolean;
                    value.value_bool = &qualified_name == "true";
                }
            } else if value.code.is_empty() {
                if let Some(expression_tree::Expression::BoolLiteral(v)) = def_val {
                    value.value_bool = *v;
                }
                value.kind = PropertyValueKind::Boolean;
            }
        }
        Type::String => {
            if let Some(expression) = &expression {
                if let Some(text) = expression
                    .child_text(SyntaxKind::StringLiteral)
                    .and_then(|s| i_slint_compiler::literals::unescape_string(&s))
                {
                    value.kind = PropertyValueKind::String;
                    value.value_string = text.as_str().into();
                } else if let Some(tr_node) = &expression.AtTr() {
                    extract_tr_data(tr_node, &mut value)
                }
            } else if value.code.is_empty() {
                if let Some(expression_tree::Expression::StringLiteral(v)) = def_val {
                    value.value_string = v.as_str().into();
                }
                value.kind = PropertyValueKind::String;
            }
        }
        Type::Enumeration(enumeration) => {
            value.kind = PropertyValueKind::Enum;
            value.value_string = enumeration.name.as_str().into();
            value.default_selection = i32::try_from(enumeration.default_value).unwrap_or_default();
            value.visual_items = Rc::new(VecModel::from(
                enumeration
                    .values
                    .iter()
                    .map(|s| SharedString::from(s.as_str()))
                    .collect::<Vec<_>>(),
            ))
            .into();

            if let Some(expression) = expression {
                if let Some(text) = expression
                    .child_node(SyntaxKind::QualifiedName)
                    .map(|n| i_slint_compiler::object_tree::QualifiedTypeName::from_node(n.into()))
                    .map(|n| {
                        let n_str = n.to_string();
                        n_str
                            .strip_prefix(&format!("{}.", enumeration.name))
                            .map(|s| s.to_string())
                            .unwrap_or(n_str)
                    })
                    .map(|s| s.to_string())
                {
                    value.value_int = enumeration
                        .values
                        .iter()
                        .position(|v| v == &text)
                        .and_then(|v| i32::try_from(v).ok())
                        .unwrap_or_default();
                }
            } else if let Some(expression_tree::Expression::EnumerationValue(v)) = def_val {
                value.value_int = v.value as i32
            }
        }
        _ => {}
    }

    value
}

fn map_property_definition(
    defined_at: &Option<properties::DefinitionInformation>,
) -> Option<PropertyDefinition> {
    let da = defined_at.as_ref()?;

    Some(PropertyDefinition {
        definition_range: to_ui_range(da.property_definition_range)?,
        selection_range: to_ui_range(da.selection_range)?,
        expression_range: to_ui_range(da.code_block_or_expression.text_range())?,
        expression_value: da.code_block_or_expression.text().to_string().into(),
    })
}

fn map_properties_to_ui(
    document_cache: &common::DocumentCache,
    properties: Option<properties::QueryPropertyResponse>,
) -> Option<(ElementInformation, HashMap<SmolStr, PropertyDeclaration>, PropertyGroupModel)> {
    use std::cmp::Ordering;

    let properties = &properties?;
    let element = properties.element.as_ref()?;

    let raw_source_uri = Url::parse(&properties.source_uri).ok()?;
    let source_uri: SharedString = raw_source_uri.to_string().into();
    let source_version = properties.source_version;

    let mut property_groups: HashMap<(SmolStr, u32), Vec<PropertyInformation>> = HashMap::new();

    let mut declarations = HashMap::new();

    fn property_group_from(
        groups: &mut HashMap<(SmolStr, u32), Vec<PropertyInformation>>,
        name: SmolStr,
        group_priority: u32,
        property: PropertyInformation,
    ) {
        let entry = groups.entry((name.clone(), group_priority));
        entry.and_modify(|e| e.push(property.clone())).or_insert(vec![property]);
    }

    for pi in &properties.properties {
        let defined_at = map_property_definition(&pi.defined_at).unwrap_or(PropertyDefinition {
            definition_range: Range { start: 0, end: 0 },
            selection_range: Range { start: 0, end: 0 },
            expression_range: Range { start: 0, end: 0 },
            expression_value: String::new().into(),
        });
        let declared_at =
            map_property_declaration(document_cache, &pi.declared_at, defined_at.clone())
                .unwrap_or(PropertyDeclaration {
                    defined_at,
                    source_path: String::new().into(),
                    source_version: -1,
                    range: Range { start: 0, end: 0 },
                });

        declarations.insert(pi.name.clone(), declared_at);

        let value = simplify_value(pi);

        property_group_from(
            &mut property_groups,
            pi.group.clone(),
            pi.group_priority,
            PropertyInformation {
                name: pi.name.as_str().into(),
                type_name: pi.ty.to_string().into(),
                value,
                display_priority: i32::try_from(pi.priority).unwrap(),
            },
        );
    }

    let keys = property_groups
        .keys()
        .sorted_by(|a, b| match a.1.cmp(&b.1) {
            Ordering::Less => Ordering::Less,
            Ordering::Equal => a.0.cmp(&b.0),
            Ordering::Greater => Ordering::Greater,
        })
        .cloned()
        .collect::<Vec<_>>();

    Some((
        ElementInformation {
            id: element.id.as_str().into(),
            type_name: element.type_name.as_str().into(),
            source_uri,
            source_version,
            range: to_ui_range(element.range)?,
        },
        declarations,
        Rc::new(VecModel::from(
            keys.iter()
                .map(|k| PropertyGroup {
                    group_name: k.0.as_str().into(),
                    properties: Rc::new(VecModel::from({
                        let mut v = property_groups.remove(k).unwrap();
                        v.sort_by(|a, b| match a.display_priority.cmp(&b.display_priority) {
                            Ordering::Less => Ordering::Less,
                            Ordering::Equal => a.name.cmp(&b.name),
                            Ordering::Greater => Ordering::Greater,
                        });
                        v
                    }))
                    .into(),
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    ))
}

fn is_equal_value(c: &PropertyValue, n: &PropertyValue) -> bool {
    c.code == n.code
}

fn is_equal_property(c: &PropertyInformation, n: &PropertyInformation) -> bool {
    c.name == n.name && c.type_name == n.type_name && is_equal_value(&c.value, &n.value)
}

fn is_equal_element(c: &ElementInformation, n: &ElementInformation) -> bool {
    c.id == n.id
        && c.type_name == n.type_name
        && c.source_uri == n.source_uri
        && c.range.start == n.range.start
}

pub type PropertyGroupModel = slint::ModelRc<PropertyGroup>;

fn update_grouped_properties(
    cvg: &VecModel<PropertyInformation>,
    nvg: &VecModel<PropertyInformation>,
) {
    enum Op {
        Insert((usize, usize)),
        Copy((usize, usize)),
        PushBack(usize),
        Remove(usize),
    }

    let mut to_do = Vec::new();

    let mut c_it = cvg.iter();
    let mut n_it = nvg.iter();

    let mut cp = c_it.next();
    let mut np = n_it.next();

    let mut c_index = 0_usize;
    let mut n_index = 0_usize;

    loop {
        match (cp.as_ref(), np.as_ref()) {
            (None, None) => break,
            (Some(_), None) => {
                to_do.push(Op::Remove(c_index));
                cp = c_it.next();
            }
            (Some(c), Some(n)) => {
                if c.name < n.name {
                    to_do.push(Op::Remove(c_index));
                    cp = c_it.next();
                } else if c.name > n.name {
                    to_do.push(Op::Insert((c_index, n_index)));
                    c_index += 1;
                    n_index += 1;
                    np = n_it.next();
                } else {
                    if !is_equal_property(c, n) {
                        to_do.push(Op::Copy((c_index, n_index)));
                    }
                    c_index += 1;
                    n_index += 1;
                    cp = c_it.next();
                    np = n_it.next();
                }
            }
            (None, Some(_)) => {
                to_do.push(Op::PushBack(n_index));
                n_index += 1;
                np = n_it.next();
            }
        }
    }

    for op in &to_do {
        match op {
            Op::Insert((c, n)) => {
                cvg.insert(*c, nvg.row_data(*n).unwrap());
            }
            Op::Copy((c, n)) => {
                cvg.set_row_data(*c, nvg.row_data(*n).unwrap());
            }
            Op::PushBack(n) => {
                cvg.push(nvg.row_data(*n).unwrap());
            }
            Op::Remove(c) => {
                cvg.remove(*c);
            }
        }
    }
}

fn get_value<T: Sized + std::convert::TryFrom<slint_interpreter::Value> + std::default::Default>(
    v: &Option<slint_interpreter::Value>,
) -> T {
    v.clone().and_then(|v| v.try_into().ok()).unwrap_or_default()
}

fn get_code(v: &Option<slint_interpreter::Value>) -> SharedString {
    v.as_ref()
        .and_then(|v| slint_interpreter::json::value_to_json(v).ok())
        .and_then(|j| serde_json::to_string_pretty(&j).ok())
        .unwrap_or_default()
        .into()
}

#[derive(Default)]
struct ValueMapping {
    name_prefix: String,
    is_too_complex: bool,
    is_array: bool,
    header: Vec<String>,
    current_values: Vec<PropertyValue>,
    array_values: Vec<Vec<PropertyValue>>,
}

fn map_value_and_type(
    ty: &langtype::Type,
    value: &Option<slint_interpreter::Value>,
    mapping: &mut ValueMapping,
) {
    use i_slint_compiler::expression_tree::Unit;
    use langtype::Type;

    eprintln!("### Mapping value {value:?} and type {ty:?}...");

    match ty {
        Type::Float32 => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Float,
                value_float: get_value::<f32>(value),
                value_string: get_value::<f32>(value).to_string().into(),
                code: get_code(value),
                ..Default::default()
            });
        }

        Type::Int32 => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Integer,
                value_int: get_value::<i32>(value),
                value_string: get_value::<i32>(value).to_string().into(),
                code: get_code(value),
                ..Default::default()
            });
        }
        Type::Duration => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Float,
                value_float: get_value::<f32>(value),
                value_string: format!("{}{}", get_value::<f32>(value), Unit::Ms).into(),
                visual_items: unit_model(&[Unit::S, Unit::Ms]),
                value_int: 0,
                code: get_code(value),
                default_selection: 1,
                ..Default::default()
            });
        }
        Type::PhysicalLength | Type::LogicalLength | Type::Rem => {
            // TODO: Is this correct? That unit is the Value anyway?!
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Float,
                value_float: get_value::<f32>(value),
                value_string: format!("{}{}", get_value::<f32>(value), Unit::Px).into(),
                visual_items: unit_model(&[
                    Unit::Px,
                    Unit::Cm,
                    Unit::Mm,
                    Unit::In,
                    Unit::Pt,
                    Unit::Phx,
                    Unit::Rem,
                ]),
                value_int: 0,
                code: get_code(value),
                default_selection: 0,
                ..Default::default()
            });
        }
        Type::Angle => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Float,
                value_float: get_value::<f32>(value),
                value_string: format!("{}{}", get_value::<f32>(value), Unit::Deg).into(),
                visual_items: unit_model(&[Unit::Deg, Unit::Grad, Unit::Turn, Unit::Rad]),
                value_int: 0,
                code: get_code(value),
                default_selection: 0,
                ..Default::default()
            });
        }
        Type::Percent => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Float,
                value_float: get_value::<f32>(value),
                value_string: format!("{}{}", get_value::<f32>(value), Unit::Percent).into(),
                visual_items: unit_model(&[Unit::Percent]),
                value_int: 0,
                code: get_code(value),
                default_selection: 0,
                ..Default::default()
            });
        }
        Type::String => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::String,
                value_string: get_value::<slint::SharedString>(value),
                code: get_code(value),
                ..Default::default()
            });
        }
        Type::Color => {
            let color = get_value::<slint::Color>(value);
            let color_string = {
                let a = color.alpha();
                let r = color.red();
                let g = color.green();
                let b = color.blue();

                format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
            };
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Color,
                value_brush: slint::Brush::SolidColor(color).into(),
                value_string: color_string.into(),
                code: get_code(value),
                ..Default::default()
            });
        }
        Type::Image => todo!(),
        Type::Bool => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Boolean,
                value_bool: get_value::<bool>(value),
                value_string: if get_value::<bool>(value) { "true".into() } else { "false".into() },
                code: get_code(value),
                ..Default::default()
            });
        }
        Type::Model => todo!(),
        Type::PathData => todo!(),
        Type::Easing => todo!(),
        Type::Brush => todo!(),
        Type::Array(sty) => {
            let mut sm = ValueMapping::default();
            sm.name_prefix = mapping.name_prefix.clone();
            map_value_and_type(sty, &None, &mut sm);

            mapping.is_too_complex = mapping.is_too_complex || sm.is_too_complex || sm.is_array;
            mapping.is_array = true;

            if !mapping.is_too_complex {
                mapping.current_values.extend_from_slice(&sm.current_values);

                let array_values = {
                    let model = get_value::<slint::ModelRc<slint_interpreter::Value>>(value);
                    model
                        .iter()
                        .map(|v| {
                            let mut sm = ValueMapping::default();
                            map_value_and_type(sty, &Some(v), &mut sm);
                            if mapping.header.is_empty() {
                                mapping.header = sm.header;
                            }
                            sm.current_values
                        })
                        .collect()
                };
                mapping.array_values = array_values;
            }
        }
        Type::Struct(s) => {
            let value_struct = get_value::<slint_interpreter::Struct>(value);

            for (f, sty) in s.fields.iter() {
                let mut sm = ValueMapping::default();
                sm.name_prefix = if mapping.name_prefix.is_empty() {
                    f.to_string()
                } else {
                    format!("{}.{f}", mapping.name_prefix)
                };
                map_value_and_type(sty, &value_struct.get_field(f).cloned(), &mut sm);

                mapping.header.extend_from_slice(&sm.header);
                mapping.current_values.extend_from_slice(&sm.current_values);
                mapping.is_too_complex = mapping.is_too_complex || sm.is_too_complex || sm.is_array;
                mapping.is_array = false;
            }
        }
        Type::Enumeration(enumeration) => todo!(),
        Type::UnitProduct(items) => todo!(),
        _ => {
            mapping.header.push(mapping.name_prefix.clone().into());
            mapping.current_values.push(PropertyValue {
                kind: PropertyValueKind::Code,
                value_string: "???".into(),
                code: get_code(value),
                ..Default::default()
            });
        }
    }

    if mapping.array_values.is_empty() {
        mapping.array_values = vec![std::mem::take(&mut mapping.current_values)];
    }

    // Back out when this got too complex and just put the JSON value in:
    if mapping.is_too_complex {
        eprintln!("      TOO COMPLEX!");
        mapping.is_array = false;
        mapping.header = vec![String::new()];
        mapping.array_values = vec![vec![PropertyValue {
            kind: PropertyValueKind::Code,
            code: get_code(value),
            ..Default::default()
        }]]
    }
}

fn map_runtime_property(rp: &runtime_properties::RuntimeProperty) -> Option<RuntimeProperty> {
    if !rp.is_property() {
        return None;
    };

    let has_getter = rp.has_getter();
    let has_setter = rp.has_setter();

    let mut mapping = ValueMapping::default();
    map_value_and_type(&rp.ty, &rp.value, &mut mapping);

    eprintln!("      >>> complete values for {}:", rp.name);
    for rnp in &mapping.array_values {
        eprintln!("          >>> New row:");

        for crp in rnp.iter() {
            eprintln!("              >>> {crp:?}");
        }
    }
    eprintln!("      >>> complete values for {}: Done", rp.name);

    let array_values = mapping
        .array_values
        .drain(..)
        .map(|v| Rc::new(slint::VecModel::from(v)).into())
        .collect::<Vec<slint::ModelRc<_>>>();
    let array_values = Rc::new(slint::VecModel::from(array_values)).into();

    Some(RuntimeProperty {
        name: rp.name.clone().into(),
        has_getter,
        has_setter,
        prefer_json: mapping.is_too_complex,
        is_array: mapping.is_array,
        header: Rc::new(VecModel::from(
            mapping.header.drain(..).map(|h| slint::SharedString::from(h)).collect::<Vec<_>>(),
        ))
        .into(),
        array_values,
    })
}

pub fn ui_set_runtime_properties(
    ui: &PreviewUi,
    runtime_properties: &HashMap<
        runtime_properties::RuntimeComponent,
        Vec<runtime_properties::RuntimeProperty>,
    >,
) {
    let mut result: Vec<RuntimeComponent> = vec![];

    fn fill_component(
        component_name: String,
        component_id: String,
        properties: &[runtime_properties::RuntimeProperty],
    ) -> Option<RuntimeComponent> {
        let properties =
            properties.iter().filter_map(|rp| map_runtime_property(&rp)).collect::<Vec<_>>();

        (!properties.is_empty()).then(|| RuntimeComponent {
            component_name: component_name.into(),
            component_id: component_id.into(),
            properties: Rc::new(slint::VecModel::from(properties)).into(),
        })
    }

    if let Some(main) = runtime_properties.get(&runtime_properties::RuntimeComponent::Main) {
        if let Some(c) = fill_component("<MAIN>".to_string(), String::new(), &main) {
            result.push(c)
        }
    }
    for component_key in
        runtime_properties.keys().filter(|k| **k != runtime_properties::RuntimeComponent::Main)
    {
        if let Some(component) = runtime_properties.get(component_key) {
            let component_key = component_key.to_string();
            if let Some(c) = fill_component(component_key.clone(), component_key, &component) {
                result.push(c);
            }
        }
    }

    let api = ui.global::<Api>();

    let mut old_model = api.get_runtime_components();

    fn update_model(
        old_model: &mut slint::ModelRc<RuntimeComponent>,
        new_model: Vec<RuntimeComponent>,
    ) -> Option<slint::ModelRc<RuntimeComponent>> {
        // The structure should never change as the API between business logic and UI should be pretty
        // fixed.

        fn m(model: Vec<RuntimeComponent>) -> Option<slint::ModelRc<RuntimeComponent>> {
            Some(Rc::new(VecModel::from(model)).into())
        }

        fn is_semantic_equal(o: &RuntimeProperty, n: &RuntimeProperty) -> bool {
            fn is_value_equal(o: &PropertyValue, n: &PropertyValue) -> bool {
                o == n
            }

            if o.name != n.name
                || o.has_getter != n.has_getter
                || o.has_setter != n.has_setter
                || o.is_array != n.is_array
                || o.prefer_json != n.prefer_json
            {
                return false;
            }
            if o.header.row_count() != n.header.row_count() {
                return false;
            }
            for (oh, nh) in o.header.iter().zip(n.header.iter()) {
                if oh != nh {
                    return false;
                }
            }

            if o.array_values.row_count() != n.array_values.row_count() {
                return false;
            }
            for (oav, nav) in o.array_values.iter().zip(n.array_values.iter()) {
                if oav.row_count() != nav.row_count() {
                    return false;
                }

                for (oavv, navv) in oav.iter().zip(nav.iter()) {
                    if !is_value_equal(&oavv, &navv) {
                        return false;
                    }
                }
            }

            true
        }

        if old_model.row_count() != new_model.len() {
            return m(new_model);
        }

        for (oc, nc) in old_model.iter().zip(new_model.iter()) {
            if oc.component_name != nc.component_name
                || oc.properties.row_count() != nc.properties.row_count()
            {
                return m(new_model);
            }
            for (i, (o, n)) in oc.properties.iter().zip(nc.properties.iter()).enumerate() {
                if o.name != n.name {
                    return m(new_model);
                }
                if !is_semantic_equal(&o, &n) {
                    eprintln!("Updating property {}@{i} of {}", o.name, oc.component_name);
                    oc.properties.set_row_data(i, n.clone());
                }
            }
        }

        None
    }

    if let Some(m) = update_model(&mut old_model, result) {
        api.set_runtime_components(m)
    }
}

fn set_runtime_property(
    component: SharedString,
    property_name: SharedString,
    model: slint::ModelRc<slint::ModelRc<PropertyValue>>,
) {
    if model.row_count() == 0 {
        eprintln!("No value given to set_runtime_property");
        return;
    }

    let component = if component.is_empty() {
        runtime_properties::RuntimeComponent::Main
    } else {
        runtime_properties::RuntimeComponent::Global(component.to_string())
    };

    // let property_value = property_value
    //     .iter()
    //     .map(|s| s.was_edited.then_some(|| s.edited_value.to_string()))
    //     .collect::<Vec<_>>();
}

fn set_runtime_json_properties(
    component: SharedString,
    property_name: SharedString,
    json_string: SharedString,
) {
    let component = if component.is_empty() {
        runtime_properties::RuntimeComponent::Main
    } else {
        runtime_properties::RuntimeComponent::Global(component.to_string())
    };
    let property_name = (!property_name.is_empty()).then_some(property_name.to_string());

    if let Err(errors) = crate::preview::set_runtime_json_properties(
        component,
        property_name,
        json_string.to_string(),
    ) {
        for e in errors {
            eprintln!("ERROR setting property value: {e}");
        }
    }
}

fn duplicate_runtime_property_value_table(
    model: slint::ModelRc<slint::ModelRc<PropertyValue>>,
) -> slint::ModelRc<slint::ModelRc<PropertyValue>> {
    let duplicate_model = model
        .iter()
        .map(|row| {
            let duplicate_row = row.iter().map(|cell| cell.clone()).collect::<Vec<_>>();
            Rc::new(slint::VecModel::from(duplicate_row)).into()
        })
        .collect::<Vec<slint::ModelRc<_>>>();
    std::rc::Rc::new(slint::VecModel::from(duplicate_model)).into()
}

fn update_properties(
    current_model: PropertyGroupModel,
    next_model: PropertyGroupModel,
) -> PropertyGroupModel {
    debug_assert_eq!(current_model.row_count(), next_model.row_count());

    for (c, n) in std::iter::zip(current_model.iter(), next_model.iter()) {
        debug_assert_eq!(c.group_name, n.group_name);

        let cvg = c.properties.as_any().downcast_ref::<VecModel<PropertyInformation>>().unwrap();
        let nvg = n.properties.as_any().downcast_ref::<VecModel<PropertyInformation>>().unwrap();

        update_grouped_properties(cvg, nvg);
    }

    current_model
}

pub fn ui_set_properties(
    ui: &PreviewUi,
    document_cache: &common::DocumentCache,
    properties: Option<properties::QueryPropertyResponse>,
) -> PropertyDeclarations {
    let (next_element, declarations, next_model) = map_properties_to_ui(document_cache, properties)
        .unwrap_or((
            ElementInformation {
                id: "".into(),
                type_name: "".into(),
                source_uri: "".into(),
                source_version: 0,
                range: Range { start: 0, end: 0 },
            },
            HashMap::new(),
            Rc::new(VecModel::from(Vec::<PropertyGroup>::new())).into(),
        ));

    let api = ui.global::<Api>();
    let current_model = api.get_properties();

    let element = api.get_current_element();
    if !is_equal_element(&element, &next_element) {
        api.set_properties(next_model);
    } else if current_model.row_count() > 0 {
        update_properties(current_model, next_model);
    } else {
        api.set_properties(next_model);
    }

    api.set_current_element(next_element);

    declarations
}

#[cfg(test)]
mod tests {
    use crate::{language::test::loaded_document_cache, preview::runtime_properties};

    use crate::common;
    use crate::preview::properties;

    use i_slint_core::model::Model;

    use super::{map_runtime_property, PropertyInformation, PropertyValue, PropertyValueKind};

    fn properties_at_position(
        source: &str,
        line: u32,
        character: u32,
    ) -> Option<(
        common::ElementRcNode,
        Vec<properties::PropertyInformation>,
        common::DocumentCache,
        lsp_types::Url,
    )> {
        let (dc, url, _) = loaded_document_cache(source.to_string());
        if let Some((e, p)) =
            properties::tests::properties_at_position_in_cache(line, character, &dc, &url)
        {
            Some((e, p, dc, url))
        } else {
            None
        }
    }

    fn property_conversion_test(contents: &str, property_line: u32) -> PropertyValue {
        let (_, pi, _, _) = properties_at_position(contents, property_line, 30).unwrap();
        let test1 = pi.iter().find(|pi| pi.name == "test1").unwrap();
        super::simplify_value(test1)
    }

    #[test]
    fn test_property_bool() {
        let result =
            property_conversion_test(r#"export component Test { in property <bool> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(!result.value_bool);
        assert!(result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <bool> test1: true; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(result.value_bool);
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <bool> test1: false; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(!result.value_bool);
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <bool> test1: 1.1.round() == 1.1.floor(); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert!(!result.value_bool);
        assert!(!result.code.is_empty());
    }

    #[test]
    fn test_property_string() {
        let result =
            property_conversion_test(r#"export component Test { in property <string> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::String);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert!(!result.value_bool);
        assert!(result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: ""; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::String);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert!(!result.value_bool);
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: "string"; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::String);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert!(!result.value_bool);
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: "" + "test"); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert!(!result.value_bool);
        assert!(!result.code.is_empty());
    }

    #[test]
    fn test_property_tr_string() {
        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: @tr("Context" => "test"); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::String);
        assert_eq!(result.value_string, "test");
        assert!(result.is_translatable);
        assert_eq!(result.tr_context, "Context");
        assert_eq!(result.tr_plural, "");
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test {
    property <int> test: 42;
    in property <string> test1: @tr("{n} string" | "{n} strings" % test);
}"#,
            2,
        );
        assert_eq!(result.kind, PropertyValueKind::String);
        assert!(result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "{n} strings");
        assert_eq!(result.tr_plural_expression, "test");
        assert_eq!(result.value_string, "{n} string");
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test {
    property <int> test: 42;
    in property <string> test1: @tr("{n} string" | "{n} strings" % self.test);
}"#,
            2,
        );
        assert_eq!(result.kind, PropertyValueKind::String);
        assert!(result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "{n} strings");
        assert_eq!(result.tr_plural_expression, "self.test");
        assert_eq!(result.value_string, "{n} string");
        assert!(!result.code.is_empty());

        // `15` is not a qualified name
        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: @tr("{n} string" | "{n} strings" % 15); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert_eq!(result.value_string, "");
        assert!(!result.code.is_empty());

        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: @tr("" + "test"); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert_eq!(result.value_string, "");
        assert!(!result.code.is_empty());
        let result = property_conversion_test(
            r#"export component Test { in property <string> test1: @tr("width {}", self.width()); }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert!(!result.is_translatable);
        assert_eq!(result.tr_context, "");
        assert_eq!(result.tr_plural, "");
        assert_eq!(result.value_string, "");
        assert!(!result.code.is_empty());
    }

    #[test]
    fn test_property_enum() {
        let result = property_conversion_test(
            r#"export component Test { in property <ImageFit> test1: ImageFit.preserve; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Enum);
        assert_eq!(result.value_string, "ImageFit");
        assert_eq!(result.value_int, 3);
        assert_eq!(result.default_selection, 0);
        assert!(!result.is_translatable);

        assert_eq!(result.visual_items.row_count(), 4);

        let result = property_conversion_test(
            r#"export component Test { in property <ImageFit> test1: ImageFit   .    /* abc */ preserve; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Enum);
        assert_eq!(result.value_string, "ImageFit");
        assert_eq!(result.value_int, 3);
        assert_eq!(result.default_selection, 0);
        assert!(!result.is_translatable);

        assert_eq!(result.visual_items.row_count(), 4);

        let result = property_conversion_test(
            r#"export component Test { in property <ImageFit> test1: /* abc */ preserve; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Enum);
        assert_eq!(result.value_string, "ImageFit");
        assert_eq!(result.value_int, 3);
        assert_eq!(result.default_selection, 0);
        assert!(!result.is_translatable);

        assert_eq!(result.visual_items.row_count(), 4);

        let result = property_conversion_test(
            r#"enum Foobar { foo, bar }
export component Test { in property <Foobar> test1: Foobar.bar; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Enum);
        assert_eq!(result.value_string, "Foobar");
        assert_eq!(result.value_int, 1);
        assert_eq!(result.default_selection, 0);
        assert!(!result.is_translatable);

        assert_eq!(result.visual_items.row_count(), 2);
        assert_eq!(result.visual_items.row_data(0), Some(slint::SharedString::from("foo")));
        assert_eq!(result.visual_items.row_data(1), Some(slint::SharedString::from("bar")));

        let result = property_conversion_test(
            r#"enum Foobar { foo, bar }
export component Test { in property <Foobar> test1; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Enum);
        assert_eq!(result.value_string, "Foobar");
        assert_eq!(result.value_int, 0); // default
        assert_eq!(result.default_selection, 0);
        assert!(!result.is_translatable);

        assert_eq!(result.visual_items.row_count(), 2);
        assert_eq!(result.visual_items.row_data(0), Some(slint::SharedString::from("foo")));
        assert_eq!(result.visual_items.row_data(1), Some(slint::SharedString::from("bar")));
    }

    #[test]
    fn test_property_float() {
        let result =
            property_conversion_test(r#"export component Test { in property <float> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 0.0);

        let result = property_conversion_test(
            r#"export component Test { in property <float> test1: 42.0; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 42.0);

        let result = property_conversion_test(
            r#"export component Test { in property <float> test1: +42.0; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 42.0);

        let result = property_conversion_test(
            r#"export component Test { in property <float> test1: -42.0; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, -42.0);

        let result = property_conversion_test(
            r#"export component Test { in property <float> test1: 42.0 * 23.0; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert_eq!(result.value_float, 0.0);
    }

    #[test]
    fn test_property_integer() {
        let result =
            property_conversion_test(r#"export component Test { in property <int> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Integer);
        assert_eq!(result.value_int, 0);

        let result = property_conversion_test(
            r#"export component Test { in property <int> test1: 42; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Integer);
        assert_eq!(result.value_int, 42);

        let result = property_conversion_test(
            r#"export component Test { in property <int> test1: +42; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Integer);
        assert_eq!(result.value_int, 42);

        let result = property_conversion_test(
            r#"export component Test { in property <int> test1: -42; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Integer);
        assert_eq!(result.value_int, -42);

        let result = property_conversion_test(
            r#"export component Test { in property <int> test1: 42 * 23; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
        assert_eq!(result.value_int, 0);
    }

    #[test]
    fn test_property_color() {
        let result =
            property_conversion_test(r#"export component Test { in property <color> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Color);
        assert!(matches!(result.value_brush, slint::Brush::SolidColor(_)));
        assert_eq!(result.value_brush.color().red(), 0);
        assert_eq!(result.value_brush.color().green(), 0);
        assert_eq!(result.value_brush.color().blue(), 0);
        assert_eq!(result.value_brush.color().alpha(), 0);

        let result = property_conversion_test(
            r#"export component Test { in property <color> test1: #10203040; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Color);
        assert!(matches!(result.value_brush, slint::Brush::SolidColor(_)));
        assert_eq!(result.value_brush.color().red(), 0x10);
        assert_eq!(result.value_brush.color().green(), 0x20);
        assert_eq!(result.value_brush.color().blue(), 0x30);
        assert_eq!(result.value_brush.color().alpha(), 0x40);

        let result = property_conversion_test(
            r#"export component Test { in property <color> test1: #10203040.darker(0.5); }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);

        let result = property_conversion_test(
            r#"export component Test { in property <color> test1: Colors.red; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
    }

    #[test]
    fn test_property_brush() {
        let result =
            property_conversion_test(r#"export component Test { in property <brush> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Brush);
        assert!(matches!(result.value_brush, slint::Brush::SolidColor(_)));
        assert_eq!(result.value_brush.color().red(), 0);
        assert_eq!(result.value_brush.color().green(), 0);
        assert_eq!(result.value_brush.color().blue(), 0);
        assert_eq!(result.value_brush.color().alpha(), 0);

        let result = property_conversion_test(
            r#"export component Test { in property <brush> test1: #10203040; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Brush);
        assert!(matches!(result.value_brush, slint::Brush::SolidColor(_)));
        assert_eq!(result.value_brush.color().red(), 0x10);
        assert_eq!(result.value_brush.color().green(), 0x20);
        assert_eq!(result.value_brush.color().blue(), 0x30);
        assert_eq!(result.value_brush.color().alpha(), 0x40);

        let result = property_conversion_test(
            r#"export component Test { in property <brush> test1: #10203040.darker(0.5); }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);

        let result = property_conversion_test(
            r#"export component Test { in property <brush> test1: Colors.red; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);

        let result = property_conversion_test(
            r#"export component Test { in property <brush> test1: @linear-gradient(90deg, #3f87a6 0%, #ebf8e1 50%, #f69d3c 100%); }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);

        let result = property_conversion_test(
            r#"export component Test { in property <brush> test1: @radial-gradient(circle, #f00 0%, #0f0 50%, #00f 100%)
            @linear-gradient(90deg, #3f87a6 0%, #ebf8e1 50%, #f69d3c 100%); }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
    }

    #[test]
    fn test_property_units() {
        let result =
            property_conversion_test(r#"export component Test { in property <length> test1; }"#, 0);
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.default_selection, 0);
        assert_eq!(result.value_int, 0);
        assert_eq!(result.visual_items.row_data(result.value_int as usize), Some("px".into()));
        let length_row_count = result.visual_items.row_count();
        assert!(length_row_count > 2);

        let result = property_conversion_test(
            r#"export component Test { in property <duration> test1: 25s; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 25.0);
        assert_eq!(result.default_selection, 0);
        assert_eq!(result.visual_items.row_data(result.value_int as usize), Some("s".into()));
        assert_eq!(result.visual_items.row_count(), 2); // ms, s

        let result = property_conversion_test(
            r#"export component Test { in property <physical-length> test1: 1.5phx; }"#,
            1,
        );
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 1.5);
        assert_eq!(result.default_selection, 0);
        assert_eq!(result.visual_items.row_data(result.value_int as usize), Some("phx".into()));
        assert!(result.visual_items.row_count() > 1); // More than just physical length

        let result = property_conversion_test(
            r#"export component Test { in property <angle> test1: 1.5turns + 1.3deg; }"#,
            0,
        );
        assert_eq!(result.kind, PropertyValueKind::Code);
    }

    #[test]
    fn test_property_with_default_values() {
        let source = r#"
import { Button } from "std-widgets.slint";
component MyButton inherits Button {
    text: "Ok";
    in property <color> color: red;
    in property alias <=> self.xxx;
    property <length> xxx: 45cm;
}
export component X {
    MyButton {
        /*CURSOR*/
    }
}
        "#;

        let (dc, url, _diag) = loaded_document_cache(source.to_string());
        let element = dc
            .element_at_offset(&url, (source.find("/*CURSOR*/").expect("cursor") as u32).into())
            .unwrap();
        let pi = super::properties::get_properties(&element, super::properties::LayoutKind::None);

        let prop = pi.iter().find(|pi| pi.name == "visible").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(result.value_bool);

        let prop = pi.iter().find(|pi| pi.name == "enabled").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(result.value_bool);

        let prop = pi.iter().find(|pi| pi.name == "text").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::String);
        assert_eq!(result.value_string, "Ok");

        let prop = pi.iter().find(|pi| pi.name == "alias").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::Float);
        assert_eq!(result.value_float, 45.);
        assert_eq!(result.visual_items.row_data(result.value_int as usize).unwrap(), "cm");

        let prop = pi.iter().find(|pi| pi.name == "color").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::Color);
        assert_eq!(
            result.value_brush,
            slint::Brush::SolidColor(slint::Color::from_rgb_u8(255, 0, 0))
        );
    }

    #[test]
    fn test_property_with_default_values_loop() {
        let source = r#"
component Abc {
        // This should be an error, not a infinite loop/hang
        in property <length> some_loop <=> r.border-width;
        r:= Rectangle {
            property <length> some_loop <=> root.some_loop;
            border-width <=> some_loop;
        }
}
export component X {
    Abc {
        /*CURSOR*/
    }
}
        "#;

        let (dc, url, _diag) = loaded_document_cache(source.to_string());

        let element = dc
            .element_at_offset(&url, (source.find("/*CURSOR*/").expect("cursor") as u32).into())
            .unwrap();
        let pi = super::properties::get_properties(&element, super::properties::LayoutKind::None);

        let prop = pi.iter().find(|pi| pi.name == "visible").unwrap();
        let result = super::simplify_value(prop);
        assert_eq!(result.kind, PropertyValueKind::Boolean);
        assert!(result.value_bool);
    }

    fn create_test_property(name: &str, value: &str) -> PropertyInformation {
        PropertyInformation {
            name: name.into(),
            display_priority: 1000,
            type_name: "Sometype".into(),
            value: PropertyValue {
                kind: PropertyValueKind::String,
                value_string: value.into(),
                code: value.into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn test_property_date_update() {
        let current = slint::VecModel::from(vec![
            create_test_property("aaa", "AAA"),
            create_test_property("bbb", "BBB"),
            create_test_property("ccc", "CCC"),
            create_test_property("ddd", "DDD"),
            create_test_property("eee", "EEE"),
        ]);
        let next = slint::VecModel::from(vec![
            create_test_property("aaa", "AAA"),
            create_test_property("aab", "AAB"),
            create_test_property("abb", "ABB"),
            create_test_property("bbb", "BBBX"),
            create_test_property("ddd", "DDD"),
        ]);

        super::update_grouped_properties(&current, &next);

        let mut it = current.iter();

        let t = it.next().unwrap();
        assert_eq!(t.name.as_str(), "aaa");
        assert_eq!(t.value.code.as_str(), "AAA");

        let t = it.next().unwrap();
        assert_eq!(t.name.as_str(), "aab");
        assert_eq!(t.value.code.as_str(), "AAB");

        let t = it.next().unwrap();
        assert_eq!(t.name.as_str(), "abb");
        assert_eq!(t.value.code.as_str(), "ABB");

        let t = it.next().unwrap();
        assert_eq!(t.name.as_str(), "bbb");
        assert_eq!(t.value.code.as_str(), "BBBX");

        let t = it.next().unwrap();
        assert_eq!(t.name.as_str(), "ddd");
        assert_eq!(t.value.code.as_str(), "DDD");

        assert!(it.next().is_none());
    }

    fn generate_runtime_property(
        visibility: &str,
        type_def: &str,
        type_name: &str,
        code: &str,
    ) -> crate::preview::runtime_properties::RuntimeProperty {
        let component_instance = crate::preview::test::interpret_test(
            "fluent",
            &format!(
                r#"
{type_def}
export component Tester {{
    {visibility} property <{type_name}> test: {code};
}}
            "#
            ),
        );
        let runtime_properties =
            runtime_properties::query_runtime_properties_and_callbacks(&component_instance);
        return runtime_properties.get(&runtime_properties::RuntimeComponent::Main).unwrap()[0]
            .clone();
    }

    fn compare_pv(r: &super::PropertyValue, e: &PropertyValue) {
        eprintln!("Received: {r:?}");
        eprintln!("Expected: {e:?}");

        assert_eq!(r.value_bool, e.value_bool);
        assert_eq!(r.is_translatable, e.is_translatable);
        assert_eq!(r.value_brush, e.value_brush);
        assert_eq!(r.value_float, e.value_float);
        assert_eq!(r.value_int, e.value_int);
        assert_eq!(r.default_selection, e.default_selection);
        assert_eq!(r.value_string, e.value_string);
        assert_eq!(r.tr_context, e.tr_context);
        assert_eq!(r.tr_plural, e.tr_plural);
        assert_eq!(r.tr_plural_expression, e.tr_plural_expression);
        assert_eq!(r.code, e.code);

        assert_eq!(r.visual_items.row_count(), e.visual_items.row_count());
        for (r, e) in r.visual_items.iter().zip(e.visual_items.iter()) {
            assert_eq!(r, e);
        }
    }

    fn validate_rp(
        visibility: &str,
        type_def: &str,
        type_name: &str,
        code: &str,
        expected: super::RuntimeProperty,
    ) {
        let rp = generate_runtime_property(visibility, type_def, type_name, code);

        let rp = map_runtime_property(&rp).unwrap();

        eprintln!("*** Validating RuntimeProperty: Received: {rp:?}");
        eprintln!("*** Validating RuntimeProperty: Expected: {expected:?}");

        assert_eq!(rp.name, expected.name);
        assert_eq!(rp.has_getter, expected.has_getter);
        assert_eq!(rp.has_setter, expected.has_setter);
        assert_eq!(rp.is_array, expected.is_array);
        assert_eq!(rp.prefer_json, expected.prefer_json);

        eprintln!("***    Basic properties all match, looking at values next...");

        assert_eq!(rp.values.row_count(), expected.values.row_count());
        for (r, e) in rp.values.iter().zip(expected.values.iter()) {
            assert_eq!(r.name, e.name);
            compare_pv(&r.value, &e.value);
        }

        eprintln!("***    Values all match, looking at array_values next...");

        assert_eq!(rp.array_values.row_count(), expected.array_values.row_count());
        for (rr, er) in rp.array_values.iter().zip(expected.array_values.iter()) {
            assert_eq!(rr.row_count(), er.row_count());
            for (e, r) in rr.iter().zip(er.iter()) {
                compare_pv(&r, &e);
            }
        }
    }

    #[test]
    fn test_map_runtime_property_string() {
        validate_rp(
            "in",
            "",
            "string",
            "\"Test\"",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "\"Test\"".into(),
                        kind: super::PropertyValueKind::String,
                        value_string: "Test".into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_length() {
        validate_rp(
            "in",
            "",
            "length",
            "100px",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "100".into(),
                        kind: super::PropertyValueKind::Float,
                        value_float: 100.0,
                        value_string: "100px".into(),
                        visual_items: std::rc::Rc::new(slint::VecModel::from(vec![
                            "px".into(),
                            "cm".into(),
                            "mm".into(),
                            "in".into(),
                            "pt".into(),
                            "phx".into(),
                            "rem".into(),
                        ]))
                        .into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_duration() {
        validate_rp(
            "in",
            "",
            "duration",
            "100s",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "100000".into(),
                        kind: super::PropertyValueKind::Float,
                        value_float: 100000.0,
                        value_string: "100000ms".into(),
                        visual_items: std::rc::Rc::new(slint::VecModel::from(vec![
                            "s".into(),
                            "ms".into(),
                        ]))
                        .into(),
                        default_selection: 1,
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_angle() {
        validate_rp(
            "in",
            "",
            "angle",
            "100turn",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "36000".into(),
                        kind: super::PropertyValueKind::Float,
                        value_float: 36000.0,
                        value_string: "36000deg".into(),
                        visual_items: std::rc::Rc::new(slint::VecModel::from(vec![
                            "deg".into(),
                            "grad".into(),
                            "turn".into(),
                            "rad".into(),
                        ]))
                        .into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_percent() {
        validate_rp(
            "in",
            "",
            "percent",
            "10%",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "10".into(),
                        kind: super::PropertyValueKind::Float,
                        value_float: 10.0,
                        value_string: "10%".into(),
                        visual_items: std::rc::Rc::new(slint::VecModel::from(vec!["%".into()]))
                            .into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_color() {
        validate_rp(
            "in",
            "",
            "color",
            "#aabbcc",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "\"#aabbccff\"".into(),
                        kind: super::PropertyValueKind::Color,
                        value_string: "#aabbccff".into(),
                        value_brush: slint::Brush::SolidColor(slint::Color::from_argb_u8(
                            0xff, 0xaa, 0xbb, 0xcc,
                        )),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_int() {
        validate_rp(
            "in",
            "",
            "int",
            "12",
            super::RuntimeProperty {
                name: "test".into(),
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "12".into(),
                        kind: super::PropertyValueKind::Integer,
                        value_string: "12".into(),
                        value_int: 12,
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_bool_true() {
        validate_rp(
            "out",
            "",
            "bool",
            "true",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "true".into(),
                        kind: super::PropertyValueKind::Boolean,
                        value_string: "true".into(),
                        value_bool: true,
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_bool_false() {
        validate_rp(
            "in-out",
            "",
            "bool",
            "false",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "false".into(),
                        kind: super::PropertyValueKind::Boolean,
                        value_string: "false".into(),
                        value_bool: false,
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_struct() {
        validate_rp(
            "in-out",
            "struct FooStruct { bar: bool, count: int }",
            "FooStruct",
            "{ bar: true, count: 23 }",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![
                    super::NamedPropertyValue {
                        name: "bar".into(),
                        value: super::PropertyValue {
                            code: "true".into(),
                            kind: super::PropertyValueKind::Boolean,
                            value_string: "true".into(),
                            value_bool: true,
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "count".into(),
                        value: super::PropertyValue {
                            code: "23".into(),
                            kind: super::PropertyValueKind::Integer,
                            value_string: "23".into(),
                            value_int: 23,
                            ..Default::default()
                        },
                    },
                ]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_struct_of_structs() {
        validate_rp(
            "in-out",
            r#"
            struct C1 { c1_1: string, c1_2: int }
            struct C2 { c2_1: string, c2_2: int }
            struct FooStruct { first: C1, second: C2 }
            "#,
            "FooStruct",
            "{ first: { c1_1: \"first of a kind\", c1_2: 23 }, second: { c2_1: \"second of a kind\", c2_2: 42 } }",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![
                    super::NamedPropertyValue {
                        name: "first.c1-1".into(),
                        value: super::PropertyValue {
                            code: "\"first of a kind\"".into(),
                            kind: super::PropertyValueKind::String,
                            value_string: "first of a kind".into(),
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "first.c1-2".into(),
                        value: super::PropertyValue {
                            code: "23".into(),
                            kind: super::PropertyValueKind::Integer,
                            value_string: "23".into(),
                            value_int: 23,
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "second.c2-1".into(),
                        value: super::PropertyValue {
                            code: "\"second of a kind\"".into(),
                            kind: super::PropertyValueKind::String,
                            value_string: "second of a kind".into(),
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "second.c2-2".into(),
                        value: super::PropertyValue {
                            code: "42".into(),
                            kind: super::PropertyValueKind::Integer,
                            value_string: "42".into(),
                            value_int: 42,
                            ..Default::default()
                        },
                    },
                ]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_bool_array() {
        validate_rp(
            "in-out",
            "",
            "[bool]",
            "[ true, false ]",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                is_array: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        code: "".into(),
                        kind: super::PropertyValueKind::Boolean,
                        value_bool: false,
                        value_string: "false".into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                array_values: std::rc::Rc::new(slint::VecModel::from(vec![
                    std::rc::Rc::new(slint::VecModel::from(vec![super::PropertyValue {
                        code: "true".into(),
                        kind: super::PropertyValueKind::Boolean,
                        value_bool: true,
                        value_string: "true".into(),
                        ..Default::default()
                    }]))
                    .into(),
                    std::rc::Rc::new(slint::VecModel::from(vec![super::PropertyValue {
                        code: "false".into(),
                        kind: super::PropertyValueKind::Boolean,
                        value_bool: false,
                        value_string: "false".into(),
                        ..Default::default()
                    }]))
                    .into(),
                ]))
                .into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_array_of_struct() {
        validate_rp(
            "in-out",
            r#"
            struct C1 { c1_1: string, c1_2: int }
            struct C2 { c2_1: string, c2_2: int }
            struct FooStruct { first: C1, second: C2 }
            "#,
            "[FooStruct]",
            "[{ first: { c1_1: \"first of a kind\", c1_2: 23 }, second: { c2_1: \"second of a kind\", c2_2: 42 } }]",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                is_array: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![
                    super::NamedPropertyValue {
                        name: "first.c1-1".into(),
                        value: super::PropertyValue {
                            kind: super::PropertyValueKind::String,
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "first.c1-2".into(),
                        value: super::PropertyValue {
                            kind: super::PropertyValueKind::Integer,
                            value_string: "0".into(),
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "second.c2-1".into(),
                        value: super::PropertyValue {
                            kind: super::PropertyValueKind::String,
                            ..Default::default()
                        },
                    },
                    super::NamedPropertyValue {
                        name: "second.c2-2".into(),
                        value: super::PropertyValue {
                            kind: super::PropertyValueKind::Integer,
                            value_string: "0".into(),
                            ..Default::default()
                        },
                    },
                ]))
                .into(),
                array_values: std::rc::Rc::new(slint::VecModel::from(vec![
                    std::rc::Rc::new(slint::VecModel::from(vec![
                        super::PropertyValue {
                            code: "\"first of a kind\"".into(),
                            kind: super::PropertyValueKind::String,
                            value_string: "first of a kind".into(),
                            ..Default::default()
                        },
                        super::PropertyValue {
                            code: "23".into(),
                            kind: super::PropertyValueKind::Integer,
                            value_string: "23".into(),
                            value_int: 23,
                            ..Default::default()
                    },
                        super::PropertyValue {
                            code: "\"second of a kind\"".into(),
                            kind: super::PropertyValueKind::String,
                            value_string: "second of a kind".into(),
                            ..Default::default()
                    },
                        super::PropertyValue {
                            code: "42".into(),
                            kind: super::PropertyValueKind::Integer,
                            value_string: "42".into(),
                            value_int: 42,
                            ..Default::default()
                    },
                    ]))
                    .into()])).into(),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_map_runtime_property_struct_with_array() {
        validate_rp(
            "in-out",
            r#"
            struct FooStruct { first: [string] }
            "#,
            "FooStruct",
            "{ first: [ \"first of a kind\", \"second of a kind\"] }",
            super::RuntimeProperty {
                name: "test".into(),
                has_getter: true,
                has_setter: true,
                prefer_json: true,
                values: std::rc::Rc::new(slint::VecModel::from(vec![super::NamedPropertyValue {
                    name: "".into(),
                    value: super::PropertyValue {
                        kind: super::PropertyValueKind::Code,
                        code: "{\n  \"first\": [\n    \"first of a kind\",\n    \"second of a kind\"\n  ]\n}".into(),
                        ..Default::default()
                    },
                }]))
                .into(),
                ..Default::default()
            },
        );
    }
}
