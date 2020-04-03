// This file is part of radicle-link
// <https://github.com/radicle-dev/radicle-link>
//
// Copyright (C) 2019-2020 The Radicle Team <dev@radicle.xyz>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License version 3 or
// later as published by the Free Software Foundation.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::thread::{Finger, ReplyTo};

/* So here's the thing... What do we do here?
 * Do we use a crdt::Map for the thread replies instead of a NonEmpty?
 * How much of the logic will be duplicated?
 * So many questions!
 */
pub trait ThreadOp<A, Op> {
    fn reply(&mut self, a: A, reply_to: ReplyTo) -> Op;
    fn delete(&mut self, finger: Finger) -> Op;
    fn edit<F: FnOnce(&mut A)>(&mut self, finger: Finger, f: F) -> Op;
}