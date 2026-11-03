// V8 Array.prototype.sort(TimSort)를 단계 그대로 옮긴다. 비교 함수가 NaN을 돌려 순서가 어긋나도 Node와 같은 결과를 낸다.
// 원본: V8 third_party/v8/builtins/array-sort.tq (CPython listsort 기반).

/// 빠른 합치기로 넘어가는 연속 승리 횟수 기본값.
const MIN_GALLOP_WINS: usize = 7;

struct SortState<'a, T, F> {
    work: &'a mut [T],
    compare: F,
    min_gallop: usize,
    runs: Vec<(usize, usize)>,
}

/// `items.sort(comparefn)`. compare가 돌려준 수의 부호만 쓰며 NaN은 0(같음)이다.
pub fn js_sort_by<T: Clone>(items: &mut [T], compare: impl FnMut(&T, &T) -> f64) {
    let length = items.len();
    if length < 2 {
        return;
    }
    let mut state = SortState { work: items, compare, min_gallop: MIN_GALLOP_WINS, runs: Vec::new() };
    let mut remaining = length;
    let mut low = 0;
    let min_run = compute_min_run_length(remaining);
    while remaining != 0 {
        let mut run = state.count_and_make_run(low, low + remaining);
        if run < min_run {
            let forced = min_run.min(remaining);
            state.binary_insertion_sort(low, low + run, low + forced);
            run = forced;
        }
        state.runs.push((low, run));
        state.merge_collapse();
        low += run;
        remaining -= run;
    }
    state.merge_force_collapse();
}

fn compute_min_run_length(mut n: usize) -> usize {
    let mut r = 0;
    while n >= 64 {
        r |= n & 1;
        n >>= 1;
    }
    n + r
}

#[derive(Clone, Copy)]
enum Source {
    Work,
    Temp,
}

impl<T: Clone, F: FnMut(&T, &T) -> f64> SortState<'_, T, F> {
    /// 음수면 a가 앞이다. NaN은 0이다.
    fn order(&mut self, a: &T, b: &T) -> f64 {
        let value = (self.compare)(a, b);
        if value.is_nan() { 0.0 } else { value }
    }

    fn count_and_make_run(&mut self, low_arg: usize, high: usize) -> usize {
        let low = low_arg + 1;
        if low == high {
            return 1;
        }
        let mut run = 2;
        let element_low = self.work[low].clone();
        let element_low_pre = self.work[low - 1].clone();
        let descending = self.order(&element_low, &element_low_pre) < 0.0;
        let mut previous = element_low;
        for index in low + 1..high {
            let current = self.work[index].clone();
            let order = self.order(&current, &previous);
            if descending {
                if order >= 0.0 {
                    break;
                }
            } else if order < 0.0 {
                break;
            }
            previous = current;
            run += 1;
        }
        if descending {
            self.work[low_arg..low_arg + run].reverse();
        }
        run
    }

    fn binary_insertion_sort(&mut self, low: usize, start_arg: usize, high: usize) {
        let mut start = if low == start_arg { start_arg + 1 } else { start_arg };
        while start < high {
            let mut left = low;
            let mut right = start;
            let pivot = self.work[start].clone();
            while left < right {
                let middle = left + ((right - left) >> 1);
                let element = self.work[middle].clone();
                if self.order(&pivot, &element) < 0.0 {
                    right = middle;
                } else {
                    left = middle + 1;
                }
            }
            self.work[left..=start].rotate_right(1);
            start += 1;
        }
    }

    fn run_invariant_established(&self, n: usize) -> bool {
        if n < 2 {
            return true;
        }
        self.runs[n - 2].1 > self.runs[n - 1].1 + self.runs[n].1
    }

    fn merge_collapse(&mut self) {
        while self.runs.len() > 1 {
            let mut n = self.runs.len() - 2;
            if !self.run_invariant_established(n + 1) || !self.run_invariant_established(n) {
                if self.runs[n - 1].1 < self.runs[n + 1].1 {
                    n -= 1;
                }
                self.merge_at(n);
            } else if self.runs[n].1 <= self.runs[n + 1].1 {
                self.merge_at(n);
            } else {
                break;
            }
        }
    }

    fn merge_force_collapse(&mut self) {
        while self.runs.len() > 1 {
            let mut n = self.runs.len() - 2;
            if n > 0 && self.runs[n - 1].1 < self.runs[n + 1].1 {
                n -= 1;
            }
            self.merge_at(n);
        }
    }

    fn merge_at(&mut self, i: usize) {
        let (mut base_a, mut length_a) = self.runs[i];
        let (base_b, mut length_b) = self.runs[i + 1];
        self.runs[i].1 = length_a + length_b;
        self.runs.remove(i + 1);

        let key_right = self.work[base_b].clone();
        let k = self.gallop_right(Source::Work, &[], &key_right, base_a, length_a, 0);
        base_a += k;
        length_a -= k;
        if length_a == 0 {
            return;
        }
        let key_left = self.work[base_a + length_a - 1].clone();
        length_b = self.gallop_left(Source::Work, &[], &key_left, base_b, length_b, length_b - 1);
        if length_b == 0 {
            return;
        }
        if length_a <= length_b {
            self.merge_low(base_a, length_a, base_b, length_b);
        } else {
            self.merge_high(base_a, length_a, base_b, length_b);
        }
    }

    fn load(&self, source: Source, temp: &[T], index: usize) -> T {
        match source {
            Source::Work => self.work[index].clone(),
            Source::Temp => temp[index].clone(),
        }
    }

    /// key가 들어갈 가장 왼쪽 자리(같은 값들 앞). V8 GallopLeft.
    fn gallop_left(&mut self, source: Source, temp: &[T], key: &T, base: usize, length: usize, hint: usize) -> usize {
        let mut last_offset: isize = 0;
        let mut offset: isize = 1;
        let hint_element = self.load(source, temp, base + hint);
        let (hint_i, length_i) = (hint as isize, length as isize);
        if self.order(&hint_element, key) < 0.0 {
            let max_offset = length_i - hint_i;
            while offset < max_offset {
                let element = self.load(source, temp, base + (hint_i + offset) as usize);
                if self.order(&element, key) >= 0.0 {
                    break;
                }
                last_offset = offset;
                offset = (offset << 1) + 1;
            }
            offset = offset.min(max_offset);
            last_offset += hint_i;
            offset += hint_i;
        } else {
            let max_offset = hint_i + 1;
            while offset < max_offset {
                let element = self.load(source, temp, base + (hint_i - offset) as usize);
                if self.order(&element, key) < 0.0 {
                    break;
                }
                last_offset = offset;
                offset = (offset << 1) + 1;
            }
            offset = offset.min(max_offset);
            let previous = last_offset;
            last_offset = hint_i - offset;
            offset = hint_i - previous;
        }
        last_offset += 1;
        while last_offset < offset {
            let middle = last_offset + ((offset - last_offset) >> 1);
            let element = self.load(source, temp, base + middle as usize);
            if self.order(&element, key) < 0.0 {
                last_offset = middle + 1;
            } else {
                offset = middle;
            }
        }
        offset as usize
    }

    /// key가 들어갈 가장 오른쪽 자리(같은 값들 뒤). V8 GallopRight.
    fn gallop_right(&mut self, source: Source, temp: &[T], key: &T, base: usize, length: usize, hint: usize) -> usize {
        let mut last_offset: isize = 0;
        let mut offset: isize = 1;
        let hint_element = self.load(source, temp, base + hint);
        let (hint_i, length_i) = (hint as isize, length as isize);
        if self.order(key, &hint_element) < 0.0 {
            let max_offset = hint_i + 1;
            while offset < max_offset {
                let element = self.load(source, temp, base + (hint_i - offset) as usize);
                if self.order(key, &element) >= 0.0 {
                    break;
                }
                last_offset = offset;
                offset = (offset << 1) + 1;
            }
            offset = offset.min(max_offset);
            let previous = last_offset;
            last_offset = hint_i - offset;
            offset = hint_i - previous;
        } else {
            let max_offset = length_i - hint_i;
            while offset < max_offset {
                let element = self.load(source, temp, base + (hint_i + offset) as usize);
                if self.order(key, &element) < 0.0 {
                    break;
                }
                last_offset = offset;
                offset = (offset << 1) + 1;
            }
            offset = offset.min(max_offset);
            last_offset += hint_i;
            offset += hint_i;
        }
        last_offset += 1;
        while last_offset < offset {
            let middle = last_offset + ((offset - last_offset) >> 1);
            let element = self.load(source, temp, base + middle as usize);
            if self.order(key, &element) < 0.0 {
                offset = middle;
            } else {
                last_offset = middle + 1;
            }
        }
        offset as usize
    }

    fn merge_low(&mut self, base_a: usize, length_a_arg: usize, base_b: usize, length_b_arg: usize) {
        let mut length_a = length_a_arg;
        let mut length_b = length_b_arg;
        let temp: Vec<T> = self.work[base_a..base_a + length_a].to_vec();
        let mut dest = base_a;
        let mut cursor_temp = 0;
        let mut cursor_b = base_b;
        self.work[dest] = self.work[cursor_b].clone();
        dest += 1;
        cursor_b += 1;

        // 'succeed: A의 나머지를 옮긴다. 'copy_b: B를 옮기고 A의 마지막 하나를 맨 뒤에 둔다.
        let copy_b = 'merge: {
            length_b -= 1;
            if length_b == 0 {
                break 'merge false;
            }
            if length_a == 1 {
                break 'merge true;
            }
            let mut min_gallop = self.min_gallop;
            loop {
                let mut wins_a = 0;
                let mut wins_b = 0;
                loop {
                    let (b, a) = (self.work[cursor_b].clone(), temp[cursor_temp].clone());
                    if self.order(&b, &a) < 0.0 {
                        self.work[dest] = b;
                        dest += 1;
                        cursor_b += 1;
                        wins_b += 1;
                        length_b -= 1;
                        wins_a = 0;
                        if length_b == 0 {
                            break 'merge false;
                        }
                        if wins_b >= min_gallop {
                            break;
                        }
                    } else {
                        self.work[dest] = a;
                        dest += 1;
                        cursor_temp += 1;
                        wins_a += 1;
                        length_a -= 1;
                        wins_b = 0;
                        if length_a == 1 {
                            break 'merge true;
                        }
                        if wins_a >= min_gallop {
                            break;
                        }
                    }
                }
                min_gallop += 1;
                let mut first = true;
                while wins_a >= MIN_GALLOP_WINS || wins_b >= MIN_GALLOP_WINS || first {
                    first = false;
                    min_gallop = min_gallop.saturating_sub(1).max(1);
                    self.min_gallop = min_gallop;

                    let key = self.work[cursor_b].clone();
                    wins_a = self.gallop_right(Source::Temp, &temp, &key, cursor_temp, length_a, 0);
                    if wins_a > 0 {
                        self.work[dest..dest + wins_a].clone_from_slice(&temp[cursor_temp..cursor_temp + wins_a]);
                        dest += wins_a;
                        cursor_temp += wins_a;
                        length_a -= wins_a;
                        if length_a == 1 {
                            break 'merge true;
                        }
                        if length_a == 0 {
                            break 'merge false;
                        }
                    }
                    self.work[dest] = self.work[cursor_b].clone();
                    dest += 1;
                    cursor_b += 1;
                    length_b -= 1;
                    if length_b == 0 {
                        break 'merge false;
                    }

                    let key = temp[cursor_temp].clone();
                    wins_b = self.gallop_left(Source::Work, &[], &key, cursor_b, length_b, 0);
                    if wins_b > 0 {
                        for offset in 0..wins_b {
                            self.work[dest + offset] = self.work[cursor_b + offset].clone();
                        }
                        dest += wins_b;
                        cursor_b += wins_b;
                        length_b -= wins_b;
                        if length_b == 0 {
                            break 'merge false;
                        }
                    }
                    self.work[dest] = temp[cursor_temp].clone();
                    dest += 1;
                    cursor_temp += 1;
                    length_a -= 1;
                    if length_a == 1 {
                        break 'merge true;
                    }
                }
                min_gallop += 1;
                self.min_gallop = min_gallop;
            }
        };
        if copy_b {
            for offset in 0..length_b {
                self.work[dest + offset] = self.work[cursor_b + offset].clone();
            }
            self.work[dest + length_b] = temp[cursor_temp].clone();
        } else if length_a > 0 {
            self.work[dest..dest + length_a].clone_from_slice(&temp[cursor_temp..cursor_temp + length_a]);
        }
    }

    fn merge_high(&mut self, base_a: usize, length_a_arg: usize, base_b: usize, length_b_arg: usize) {
        let mut length_a = length_a_arg;
        let mut length_b = length_b_arg;
        let temp: Vec<T> = self.work[base_b..base_b + length_b].to_vec();
        // 뒤에서 앞으로 합친다. 커서는 0 아래로 내려갈 수 있어 부호 있는 정수로 둔다.
        let mut dest = (base_b + length_b - 1) as isize;
        let mut cursor_temp = length_b as isize - 1;
        let mut cursor_a = (base_a + length_a - 1) as isize;
        let at = |index: isize| index as usize;
        self.work[at(dest)] = self.work[at(cursor_a)].clone();
        dest -= 1;
        cursor_a -= 1;

        // 'succeed: B(임시)의 나머지를 앞쪽에 옮긴다. 'copy_a: A를 옮기고 B의 첫 하나를 맨 앞에 둔다.
        let copy_a = 'merge: {
            length_a -= 1;
            if length_a == 0 {
                break 'merge false;
            }
            if length_b == 1 {
                break 'merge true;
            }
            let mut min_gallop = self.min_gallop;
            loop {
                let mut wins_a = 0;
                let mut wins_b = 0;
                loop {
                    let (b, a) = (temp[at(cursor_temp)].clone(), self.work[at(cursor_a)].clone());
                    if self.order(&b, &a) < 0.0 {
                        self.work[at(dest)] = a;
                        dest -= 1;
                        cursor_a -= 1;
                        wins_a += 1;
                        length_a -= 1;
                        wins_b = 0;
                        if length_a == 0 {
                            break 'merge false;
                        }
                        if wins_a >= min_gallop {
                            break;
                        }
                    } else {
                        self.work[at(dest)] = b;
                        dest -= 1;
                        cursor_temp -= 1;
                        wins_b += 1;
                        length_b -= 1;
                        wins_a = 0;
                        if length_b == 1 {
                            break 'merge true;
                        }
                        if wins_b >= min_gallop {
                            break;
                        }
                    }
                }
                min_gallop += 1;
                let mut first = true;
                while wins_a >= MIN_GALLOP_WINS || wins_b >= MIN_GALLOP_WINS || first {
                    first = false;
                    min_gallop = min_gallop.saturating_sub(1).max(1);
                    self.min_gallop = min_gallop;

                    let key = temp[at(cursor_temp)].clone();
                    let k = self.gallop_right(Source::Work, &[], &key, base_a, length_a, length_a - 1);
                    wins_a = length_a - k;
                    if wins_a > 0 {
                        dest -= wins_a as isize;
                        cursor_a -= wins_a as isize;
                        for offset in (0..wins_a).rev() {
                            self.work[at(dest + 1) + offset] = self.work[at(cursor_a + 1) + offset].clone();
                        }
                        length_a -= wins_a;
                        if length_a == 0 {
                            break 'merge false;
                        }
                    }
                    self.work[at(dest)] = temp[at(cursor_temp)].clone();
                    dest -= 1;
                    cursor_temp -= 1;
                    length_b -= 1;
                    if length_b == 1 {
                        break 'merge true;
                    }

                    let key = self.work[at(cursor_a)].clone();
                    let k = self.gallop_left(Source::Temp, &temp, &key, 0, length_b, length_b - 1);
                    wins_b = length_b - k;
                    if wins_b > 0 {
                        dest -= wins_b as isize;
                        cursor_temp -= wins_b as isize;
                        self.work[at(dest + 1)..at(dest + 1) + wins_b]
                            .clone_from_slice(&temp[at(cursor_temp + 1)..at(cursor_temp + 1) + wins_b]);
                        length_b -= wins_b;
                        if length_b == 1 {
                            break 'merge true;
                        }
                        if length_b == 0 {
                            break 'merge false;
                        }
                    }
                    self.work[at(dest)] = self.work[at(cursor_a)].clone();
                    dest -= 1;
                    cursor_a -= 1;
                    length_a -= 1;
                    if length_a == 0 {
                        break 'merge false;
                    }
                }
                min_gallop += 1;
                self.min_gallop = min_gallop;
            }
        };
        if copy_a {
            dest -= length_a as isize;
            cursor_a -= length_a as isize;
            for offset in (0..length_a).rev() {
                self.work[at(dest + 1) + offset] = self.work[at(cursor_a + 1) + offset].clone();
            }
            self.work[at(dest)] = temp[at(cursor_temp)].clone();
        } else if length_b > 0 {
            let start = at(dest - (length_b as isize - 1));
            self.work[start..start + length_b].clone_from_slice(&temp[..length_b]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::js_sort_by;

    /// /tmp/office-rs-compare/r17-sort-cases.mjs가 Node 24(V8)에서 낸 정렬 결과 해시.
    const NODE_HASHES: [u32; 80] = [
        3767693382, 2708726504, 1199222974, 3616667616, 791355183, 1169331528, 633272097, 4122545889, 1421775578, 3282719153,
        469215875, 1025656189, 1804324277, 1605819750, 3509527578, 4141748885, 109770887, 4006404386, 2153402255, 3066798695,
        636763192, 1759560106, 2999818129, 1288366901, 3727953613, 1872035673, 3349959092, 1711300685, 539528703, 2039106730,
        3385458028, 626033953, 299036804, 341342200, 902745164, 99752350, 1634952725, 470954398, 3876414530, 1877446280,
        351484151, 2631527573, 1738010646, 3357479305, 2429744160, 202570650, 227588888, 29177271, 3426551440, 1511684975,
        3973886043, 922350635, 1216892871, 1011977377, 2924185309, 2015366562, 3296281596, 2777196674, 2400686695, 3769302202,
        2832015594, 1775260454, 1923430489, 3343953623, 717557274, 917348841, 1395343143, 984102201, 3003247361, 1279034529,
        2450767830, 1147699731, 1368985105, 335998656, 804297245, 1627912814, 2531608478, 900162165, 992730308, 431745910,
    ];

    #[test]
    fn nan이_섞인_비교도_v8과_같은_순서로_정렬한다() {
        let mut state = 0x9e37_79b9_u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for (case, expected) in NODE_HASHES.iter().enumerate() {
            let n = if case < 20 { 1 + next() % 70 } else { 1 + next() % 1200 };
            let kind = case % 5;
            let mut items: Vec<(u32, f64)> = Vec::new();
            for i in 0..n {
                let nan = next() % 100 < 12;
                let order = match kind {
                    0 => f64::from(next() % 11) - 5.0,
                    1 => f64::from(next() % 2001) - 1000.0,
                    2 => f64::from(i + next() % 3),
                    3 => f64::from(n) - f64::from(i) - f64::from(next() % 3),
                    _ => f64::from((i / 50) % 4),
                };
                items.push((i, if nan { f64::NAN } else { order }));
            }
            js_sort_by(&mut items, |a, b| b.1 - a.1);
            let hash = items.iter().fold(2_166_136_261_u32, |hash, (i, _)| (hash ^ i).wrapping_mul(16_777_619));
            assert_eq!(hash, *expected, "{case}번 경우(n={n}, 종류 {kind})");
        }
    }

    #[test]
    fn 일관된_비교는_안정_정렬과_같다() {
        let mut items: Vec<(usize, i32)> = (0..500).map(|i| (i, ((i * 7919) % 13) as i32)).collect();
        let mut expected = items.clone();
        expected.sort_by_key(|item| -item.1);
        js_sort_by(&mut items, |a, b| f64::from(b.1 - a.1));
        assert_eq!(items, expected);
    }
}
