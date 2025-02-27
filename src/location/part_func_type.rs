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

#[derive(Clone, Debug, PartialEq)]
pub enum PartFuncType {
    UNKNOWN = -1,
    HASH = 0,
    KEY = 1,
    KeyImplicit = 2,
    RANGE = 3,
    RangeColumns = 4,
    LIST = 5,
    KeyV2 = 6,
    ListColumns = 7,
    HashV2 = 8,
    KeyV3 = 9,
}

impl PartFuncType {
    pub fn from_i32(index: i32) -> PartFuncType {
        match index {
            0 => PartFuncType::HASH,
            1 => PartFuncType::KEY,
            2 => PartFuncType::KeyImplicit,
            3 => PartFuncType::RANGE,
            4 => PartFuncType::RangeColumns,
            5 => PartFuncType::LIST,
            6 => PartFuncType::KeyV2,
            7 => PartFuncType::ListColumns,
            8 => PartFuncType::HashV2,
            9 => PartFuncType::KeyV3,
            _ => PartFuncType::UNKNOWN,
        }
    }

    pub fn is_list_part(&self) -> bool {
        matches!(self, PartFuncType::LIST | PartFuncType::ListColumns)
    }

    pub fn is_key_part(&self) -> bool {
        matches!(self, PartFuncType::KeyImplicit | PartFuncType::KeyV2 | PartFuncType::KeyV3)
    }

    pub fn is_range_part(&self) -> bool {
        matches!(self, PartFuncType::RANGE | PartFuncType::RangeColumns)
    }

    pub fn is_hash_part(&self) -> bool {
        matches!(self, PartFuncType::HASH | PartFuncType::HashV2)
    }
}
