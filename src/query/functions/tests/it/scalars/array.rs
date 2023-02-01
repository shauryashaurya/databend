// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write;

use common_expression::types::*;
use common_expression::FromData;
use goldenfile::Mint;

use super::run_ast;

#[test]
fn test_array() {
    let mut mint = Mint::new("tests/it/scalars/testdata");
    let file = &mut mint.new_goldenfile("array.txt").unwrap();

    test_create(file);
    test_length(file);
    test_get(file);
    test_slice(file);
    test_remove_first(file);
    test_remove_last(file);
    test_contains(file);
    test_concat(file);
    test_prepend(file);
    test_append(file);
    test_indexof(file);
}

fn test_create(file: &mut impl Write) {
    run_ast(file, "[]", &[]);
    run_ast(file, "[NULL, 8, -10]", &[]);
    run_ast(file, "[['a', 'b'], []]", &[]);
    run_ast(file, r#"['a', 1, parse_json('{"foo":"bar"}')]"#, &[]);
    run_ast(
        file,
        r#"[parse_json('[]'), parse_json('{"foo":"bar"}')]"#,
        &[],
    );
}

fn test_length(file: &mut impl Write) {
    run_ast(file, "length([])", &[]);
    run_ast(file, "length([1, 2, 3])", &[]);
    run_ast(file, "length([true, false])", &[]);
    run_ast(file, "length(['a', 'b', 'c', 'd'])", &[]);
}

fn test_get(file: &mut impl Write) {
    run_ast(file, "[1, 2]['a']", &[]);
    run_ast(file, "[][1]", &[]);
    run_ast(file, "[][NULL]", &[]);
    run_ast(file, "[true, false][1]", &[]);
    run_ast(file, "['a', 'b', 'c'][2]", &[]);
    run_ast(file, "[1, 2, 3][1]", &[]);
    run_ast(file, "[1, 2, 3][3]", &[]);
    run_ast(file, "[1, null, 3][1]", &[]);
    run_ast(file, "[1, null, 3][2]", &[]);
    run_ast(file, "[1, 2, 3][4]", &[]);
    run_ast(file, "[a, b][idx]", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
        ("idx", UInt16Type::from_data(vec![1u16, 2, 3])),
    ]);
}

fn test_slice(file: &mut impl Write) {
    run_ast(file, "slice([], 1, 2)", &[]);
    run_ast(file, "slice([1], 1, 2)", &[]);
    run_ast(file, "slice([NULL, 1, 2, 3], 0, 2)", &[]);
    run_ast(file, "slice([0, 1, 2, 3], 1, 2)", &[]);
    run_ast(file, "slice([0, 1, 2, 3], 1, 5)", &[]);
    run_ast(file, "slice(['a', 'b', 'c', 'd'], 0, 2)", &[]);
    run_ast(file, "slice(['a', 'b', 'c', 'd'], 1, 4)", &[]);
    run_ast(file, "slice(['a', 'b', 'c', 'd'], 2, 6)", &[]);
    run_ast(file, "slice([a, b, c], 1, 2)", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
        ("c", Int16Type::from_data(vec![7i16, 8, 9])),
    ]);
}

fn test_remove_first(file: &mut impl Write) {
    run_ast(file, "remove_first([])", &[]);
    run_ast(file, "remove_first([1])", &[]);
    run_ast(file, "remove_first([0, 1, 2, NULL])", &[]);
    run_ast(file, "remove_first([0, 1, 2, 3])", &[]);
    run_ast(file, "remove_first(['a', 'b', 'c', 'd'])", &[]);
    run_ast(file, "remove_first([a, b])", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
    ]);
}

fn test_remove_last(file: &mut impl Write) {
    run_ast(file, "remove_last([])", &[]);
    run_ast(file, "remove_last([1])", &[]);
    run_ast(file, "remove_last([0, 1, 2, NULL])", &[]);
    run_ast(file, "remove_last([0, 1, 2, 3])", &[]);
    run_ast(file, "remove_last(['a', 'b', 'c', 'd'])", &[]);
    run_ast(file, "remove_last([a, b])", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
    ]);
}

fn test_contains(file: &mut impl Write) {
    run_ast(file, "false in (false, true)", &[]);
    run_ast(file, "'33' in ('1', '33', '23', '33')", &[]);
    run_ast(file, "contains([1,2,3], 2)", &[]);

    let columns = [
        ("int8_col", Int8Type::from_data(vec![1i8, 2, 7, 8])),
        (
            "nullable_col",
            Int64Type::from_data_with_validity(vec![9i64, 10, 11, 12], vec![
                true, true, false, false,
            ]),
        ),
    ];

    run_ast(file, "int8_col not in (1, 2, 3, 4, 5, null)", &columns);
    run_ast(file, "contains([1,2,null], nullable_col)", &columns);
    run_ast(
        file,
        "contains([(1,'2', 3, false), (1,'2', 4, true), null], (1,'2', 3, false))",
        &columns,
    );
    run_ast(file, "nullable_col in (null, 9, 10, 12)", &columns);
    run_ast(
        file,
        "nullable_col in (1, '9', 3, 10, 12, true, [1,2,3])",
        &columns,
    );
}

fn test_concat(file: &mut impl Write) {
    run_ast(file, "concat([], [])", &[]);
    run_ast(file, "concat([], [1,2])", &[]);
    run_ast(file, "concat([false, true], [])", &[]);
    run_ast(file, "concat([false, true], [1,2])", &[]);
    run_ast(file, "concat([1,2,3], ['s', null])", &[]);

    let columns = [
        ("int8_col", Int8Type::from_data(vec![1i8, 2, 7, 8])),
        (
            "nullable_col",
            Int64Type::from_data_with_validity(vec![9i64, 10, 11, 12], vec![
                true, true, false, false,
            ]),
        ),
    ];

    run_ast(
        file,
        "concat([1, 2, 3, 4, 5, null], [nullable_col])",
        &columns,
    );
    run_ast(file, "concat([1,2,null], [int8_col])", &columns);
}

fn test_prepend(file: &mut impl Write) {
    run_ast(file, "prepend(1, [])", &[]);
    run_ast(file, "prepend(1, [2, 3, NULL, 4])", &[]);
    run_ast(file, "prepend('a', ['b', NULL, NULL, 'c', 'd'])", &[]);
    run_ast(file, "prepend(a, [b, c])", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
        ("c", Int16Type::from_data(vec![6i16, 7, 8])),
    ]);
}

fn test_append(file: &mut impl Write) {
    run_ast(file, "append([], 1)", &[]);
    run_ast(file, "append([2, 3, NULL, 4], 5)", &[]);
    run_ast(file, "append(['b', NULL, NULL, 'c', 'd'], 'e')", &[]);
    run_ast(file, "append([b, c], a)", &[
        ("a", Int16Type::from_data(vec![0i16, 1, 2])),
        ("b", Int16Type::from_data(vec![3i16, 4, 5])),
        ("c", Int16Type::from_data(vec![6i16, 7, 8])),
    ]);
}

fn test_indexof(file: &mut impl Write) {
    run_ast(file, "indexof([false, true], false)", &[]);
    run_ast(file, "indexof([], false)", &[]);
    run_ast(file, "indexof([false, true], null)", &[]);
    run_ast(file, "indexof([false, true], 0)", &[]);
    run_ast(file, "indexof([1,2,3,'s'], 's')", &[]);
    run_ast(file, "indexof([1,'x',null,'x'], 'x')", &[]);

    let columns = [
        ("int8_col", Int8Type::from_data(vec![1i8, 2, 7, 8])),
        (
            "nullable_col",
            Int64Type::from_data_with_validity(vec![9i64, 10, 11, 12], vec![
                true, true, false, false,
            ]),
        ),
    ];

    run_ast(
        file,
        "indexof([1, 2, 3, 4, 5, null], nullable_col)",
        &columns,
    );
    run_ast(file, "indexof([9,10,null], int8_col)", &columns);
}
