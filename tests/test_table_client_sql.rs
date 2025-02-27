/*-
 * #%L
 * OBKV Table Client Framework
 * %%
 * Copyright (C) 2021 OceanBase
 * %%
 * OBKV Table Client Framework is licensed under Mulan PSL v2.
 * You can use this software according to the terms and conditions of the Mulan PSL v2.
 * You may obtain a copy of Mulan PSL v2 at:
 *          http://license.coscl.org.cn/MulanPSL2
 * THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY KIND,
 * EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO NON-INFRINGEMENT,
 * MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
 * See the Mulan PSL v2 for more details.
 * #L%
 */

#[allow(unused_imports)]
#[allow(unused)]
mod utils;

use obkv::{Table, Value};

#[test]
fn test_execute_sql() {
    let client = utils::common::build_normal_client();
    let test_table_name = "test_execute_sql";
    let create_table =
        format!("create table IF NOT EXISTS {}(id int, PRIMARY KEY(id));", test_table_name);
    client
        .execute_sql(&create_table)
        .expect("fail to create table");
}

#[test]
fn test_check_table_exists() {
    let client = utils::common::build_normal_client();
    let test_table_name = "test_check_table_exists";
    let drop_table = format!("drop table IF EXISTS {};", test_table_name);

    client
        .execute_sql(&drop_table)
        .expect("fail to create table");

    let exists = client
        .check_table_exists(test_table_name)
        .expect("fail to check table exists");
    assert!(!exists, "should not exists");

    let create_table = format!(
        "create table IF NOT EXISTS {}(id int, PRIMARY KEY(id));",
        test_table_name
    );

    client
        .execute_sql(&create_table)
        .expect("fail to create table");

    let exists = client
        .check_table_exists(test_table_name)
        .expect("fail to check table exists");
    assert!(exists, "should exists");
}

#[test]
fn test_truncate_table() {
    let client = utils::common::build_normal_client();
    let test_table_name = "test_varchar_table";
    let truncate_once = || {
        client
            .truncate_table(test_table_name)
            .expect("Fail to truncate first test table");

        let result = client
            .get(
                test_table_name,
                vec![Value::from("foo")],
                vec!["c2".to_owned()],
            )
            .expect("Fail to get row");
        assert!(result.is_empty());

        let result = client
            .insert(
                test_table_name,
                vec![Value::from("foo")],
                vec!["c2".to_owned()],
                vec![Value::from("bar")],
            )
            .expect("Fail to insert row");
        assert_eq!(result, 1);

        client
            .truncate_table(test_table_name)
            .expect("Fail to truncate first test table");

        let result = client
            .get(
                test_table_name,
                vec![Value::from("foo")],
                vec!["c2".to_owned()],
            )
            .expect("Fail to get row");
        assert!(result.is_empty());
    };
    for _ in 0..1 {
        truncate_once();
    }
}