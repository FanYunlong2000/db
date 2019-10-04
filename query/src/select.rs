use chrono::NaiveDate;
use unchecked_unwrap::UncheckedUnwrap;
use csv::Writer;

use common::{*, BareTy::*, Error::*, AggOp::*};
use syntax::ast::*;
use physics::*;
use db::Db;
use crate::{predicate::{and, one_predicate, cross_predicate}, filter::filter, is_null};

#[derive(Copy, Clone)]
pub struct Col {
  // if op == Some(CountAll), `idx` is a meaningless value, `ci` is None, or `ci` will always be Some
  pub op: Option<AggOp>,
  // index of ci in table page, used to access null bit
  pub idx: u32,
  pub ci: Option<&'static ColInfo>,
}

pub struct SelectResult {
  cols: Vec<Col>,
  // `data` is a 2-d array of data cell (Either<...>)
  // its col size = cols.len()
  // since Lit doesn't contain NaiveDate, we need to use an Either here
  data: Vec<LitExt<'static>>,
}

// caller col.op != CountAll (<=> col.ci.is_some())
unsafe fn ptr2lit(data: *const u8, col: &Col) -> LitExt<'static> {
  if is_null(data, col.idx as usize) { return LitExt::Null; };
  let ci = col.ci.unchecked_unwrap();
  let ptr = data.add(ci.off as usize);
  match ci.ty.ty {
    Int => LitExt::Int(*(ptr as *const i32)),
    Bool => LitExt::Bool(*(ptr as *const bool)),
    Float => LitExt::Float(*(ptr as *const f32)),
    Char | VarChar => LitExt::Str(str_from_parts(ptr.add(1), *ptr as usize)),
    Date => LitExt::Date(*(ptr as *const NaiveDate)),
  }
}

impl SelectResult {
  // `data` is 2-d array of dimension = tbls.len() * (data.len() / tbls.len())
  // tbls[i] <-> data[i], both belongs to a table
  unsafe fn new(tbls: &[Vec<Col>], data: &[*const u8]) -> SelectResult {
    debug_assert_eq!(data.len() % tbls.len(), 0);
    let result_num = data.len() / tbls.len();
    // if has agg, all col should have agg (checked in mk_tbls)
    let has_agg = tbls.iter().flatten().any(|col| col.op.is_some());
    let data = if has_agg {
      tbls.iter().enumerate().flat_map(|(idx, tbl)| {
        tbl.iter().map(move |col| {
          // avg, sum, min, max, count should ignore null, if none is not null, all except count should return null, count should return 0
          // avg's denominator should also ignore null
          // count(*) should not ignore null
          let op = col.op.unchecked_unwrap();
          match op {
            Avg | Sum => { // only accept Int, Float, Bool, checked in mk_tbls
              let mut sum = 0.0; // use f64 for better precision (cover i32)
              let mut notnull_cnt = 0;
              for i in 0..result_num {
                let data = *data.get_unchecked(i * tbls.len() + idx);
                if !is_null(data, col.idx as usize) {
                  let ci = col.ci.unchecked_unwrap();
                  let ptr = data.add(ci.off as usize);
                  match ci.ty.ty {
                    Int => sum += *(ptr as *const i32) as f64,
                    Bool => sum += *(ptr as *const bool) as i8 as f64,
                    Float => sum += *(ptr as *const f32) as f64,
                    _ => debug_unreachable!(),
                  }
                  notnull_cnt += 1;
                }
              }
              if notnull_cnt == 0 { LitExt::Null } else {
                LitExt::F64(if op == Avg { sum / notnull_cnt as f64 } else { sum })
              }
            }
            Min | Max => {
              let it = (0..result_num).filter_map(|i| {
                match ptr2lit(*data.get_unchecked(i * tbls.len() + idx), col) { LitExt::Null => None, lit => Some(lit) }
              });
              if op == Max { it.max() } else { it.min() }.unwrap_or(LitExt::Null)
            }
            Count => LitExt::Int((0..result_num).filter(|&i| {
              !is_null(*data.get_unchecked(i * tbls.len() + idx), col.idx as usize)
            }).count() as i32),
            CountAll => LitExt::Int(result_num as i32),
          }
        })
      }).collect()
    } else {
      let row = (tbls.iter().map(|tbl| tbl.len())).sum::<usize>();
      let mut ret = Vec::<LitExt>::with_capacity(result_num * row);
      ret.set_len(result_num * row);
      for i in 0..result_num {
        let mut j = 0;
        for (idx, tbl) in tbls.iter().enumerate() {
          let data = *data.get_unchecked(i * tbls.len() + idx);
          for col in tbl {
            ret.as_mut_ptr().add(i * row + j).write(ptr2lit(data, col));
            j += 1;
          }
        }
      }
      ret
    };
    SelectResult { cols: tbls.iter().flatten().copied().collect(), data }
  }

  pub fn row_count(&self) -> usize {
    debug_assert_eq!(self.data.len() % self.cols.len(), 0);
    self.data.len() / self.cols.len()
  }

  // actually I don't believe any error can happen when making csv
  // it is just because I am not familiar enough with this lib, or I will definitely use unchecked_unwrap everywhere
  pub fn to_csv<'a>(&self) -> Result<'a, String> {
    unsafe {
      let mut csv = Vec::new();
      let mut wt = Writer::from_writer(&mut csv);
      for &Col { op, ci, .. } in &self.cols {
        if let Some(ci) = ci {
          let name = ci.name();
          if let Some(op) = op { wt.write_field(format!("{}({})", op.name(), name))?; } else { wt.write_field(name)?; }
        } else {
          debug_assert!(op == Some(CountAll));
          wt.write_field("count(*)")?;
        }
      }
      wt.write_record(None::<&[u8]>)?;
      for i in 0..self.row_count() {
        let row = self.data.get_unchecked(i * self.cols.len()..(i + 1) * self.cols.len());
        wt.write_record(row.iter().map(|data| format!("{:?}", data)))?;
      }
      drop(wt);
      Ok(String::from_utf8_unchecked(csv))
    }
  }
}

struct InsertCtx<'a> {
  tbls: IndexMap<&'a str, (u32, &'a TablePage)>,
  cols: HashMap<&'a str, Option<(&'a TablePage, &'a ColInfo, usize)>>,
}

unsafe fn one_where<'a, 'b>(cr: &ColRef<'b>, ctx: &InsertCtx) -> Result<'b, (&'a TablePage, &'a ColInfo, usize)> {
  if let Some(t) = cr.table {
    if let Some((tbl_idx, _, &tp)) = ctx.tbls.get_full(t) {
      Ok((tp.1.pr(), tp.1.pr().get_ci(cr.col)?, tbl_idx))
    } else { Err(NoSuchTable(t)) }
  } else {
    match ctx.cols.get(cr.col) {
      Some(&Some((tp, ci, tbl_idx))) => Ok((tp.pr(), ci.pr(), tbl_idx)),
      Some(None) => Err(AmbiguousCol(cr.col)),
      None => Err(NoSuchCol(cr.col)),
    }
  }
}

// the validity of AggOp is checked here
unsafe fn mk_tbls<'a>(ops: &Option<Vec<Agg<'a>>>, ctx: &InsertCtx) -> Result<'a, Vec<Vec<Col>>> {
  if let Some(ops) = ops {
    if ops.iter().any(|agg| agg.op.is_some()) != ops.iter().all(|agg| agg.op.is_some()) {
      return Err(MixedSelect);
    }
    let mut ret = vec![vec![]; ctx.tbls.len()];
    for &Agg { op, col } in ops {
      if op == Some(CountAll) {
        // I admit it is quite ugly...
        debug_assert!(0 < ret.len());
        ret.get_unchecked_mut(0).push(Col { op, idx: !0, ci: None });
      } else {
        let (tp, ci, idx) = one_where(&col, ctx)?;
        debug_assert!(idx < ret.len());
        let ty = ci.ty.ty;
        if let Some(op) = op {
          if (op == Avg || op == Sum) && ty != Int && ty != Float && ty != Bool {
            return Err(InvalidAgg { col: ci.ty, op });
          }
        }
        let ci_id = ci.idx(&tp.cols);
        ret.get_unchecked_mut(idx).push(Col { op, idx: ci_id, ci: Some(ci) });
      }
    }
    Ok(ret)
  } else { // select *
    Ok(ctx.tbls.iter().map(|(_, &(_, tp))| {
      tp.cols().iter().enumerate().map(|(idx, ci)| Col { op: None, idx: idx as u32, ci: Some(ci) }).collect()
    }).collect())
  }
}

pub fn select<'a>(s: &Select<'a>, db: &mut Db) -> Result<'a, SelectResult> {
  unsafe {
    let tbl_num = s.tables.len();
    debug_assert!(tbl_num >= 1); // parser guarantee this
    let mut tbls = IndexMap::default();
    let mut cols = HashMap::new();
    for (idx, &t) in s.tables.iter().enumerate() {
      let (tp_id, tp) = db.get_tp(t)?;
      match tbls.entry(t) {
        IndexEntry::Occupied(_) => return Err(DupTable(t)),
        IndexEntry::Vacant(v) => { v.insert((tp_id, tp.prc())); }
      }
      for ci in tp.cols() {
        // if it exist, make it None; if it doesn't exist, insert it
        cols.entry(ci.name()).and_modify(|x| *x = None)
          .or_insert(Some((tp.prc(), ci, idx)));
      }
    }
    debug_assert_eq!(tbls.len(), tbl_num);
    let ctx = InsertCtx { tbls, cols };
    let result_tbls = mk_tbls(&s.ops, &ctx)?;

    let mut one_preds = Vec::with_capacity(tbl_num);
    let mut cross_preds = Vec::with_capacity(tbl_num * tbl_num); // 2-d array, dim = tbl_num * tbl_num
    for _ in 0..tbl_num { one_preds.push(vec![]); } // Box<Fn> is not Clone, so must use loop to push
    for _ in 0..tbl_num * tbl_num { cross_preds.push(vec![]); }
    let mut one_wheres = vec![vec![]; tbl_num];
    for e in &s.where_ {
      let (l, r) = (e.lhs_col(), e.rhs_col());
      let (tp_l, ci_l, tbl_idx_l) = one_where(l, &ctx)?;
      debug_assert!(tbl_idx_l < one_preds.len());
      if let Some((tp_r, ci_r, tbl_idx_r)) = {
        if let Some(r) = r {
          Some(one_where(r, &ctx)?).filter(|(_, _, tbl_idx_r)| *tbl_idx_r != tbl_idx_l)
        } else { None }
      } { // not in one table
        if let &Expr::Cmp(op, _, _) = e {
          cross_preds[tbl_idx_l * tbl_num + tbl_idx_r].push(cross_predicate(op, (ci_l, ci_r), (tp_l, tp_r))?);
        } else { debug_unreachable!() } // if expr have rhs col, it must have cmp op
      } else { // in one table
        one_preds.get_unchecked_mut(tbl_idx_l).push(one_predicate(e, tp_l)?);
        one_wheres.get_unchecked_mut(tbl_idx_l).push(e);
      }
    }
    let cross_preds = cross_preds.into_iter().map(|p| and(p)).collect::<Vec<_>>();
    let one_results = ctx.tbls.values().zip(one_preds.into_iter()).zip(one_wheres.iter())
      .map(|((&tp, pred), where_)| {
        let mut data = Vec::new();
        filter(where_, tp, db, and(pred), |x, _| data.push(x as *const u8));
        data
      }).collect::<Vec<_>>();

    let res0 = one_results.get_unchecked(0);
    let mut final_ = Vec::<*const u8>::with_capacity(res0.len() * tbl_num);
    final_.set_len(res0.len() * tbl_num);
    for (i, &x) in res0.iter().enumerate() {
      final_.as_mut_ptr().add(i * tbl_num).write(x);
    }

    for r_idx in 1..one_results.len() {
      let rs = one_results.get_unchecked(r_idx);
      let mut new_final_ = Vec::<*const u8>::new();
      for old_idx in 0..(final_.len() / tbl_num) {
        let old_row = final_.as_ptr().add(old_idx * tbl_num);
        for &r in rs {
          let ok = (0..r_idx).all(|l_idx| {
            let l = *old_row.add(l_idx);
            cross_preds.get_unchecked(l_idx * tbl_num + r_idx)((l, r)) &&
              cross_preds.get_unchecked(r_idx * tbl_num + l_idx)((r, l))
          });
          if ok {
            let old_len = new_final_.len();
            new_final_.reserve(tbl_num);
            new_final_.set_len(old_len + tbl_num);
            new_final_.as_mut_ptr().add(old_len).copy_from_nonoverlapping(old_row, tbl_num);
            *new_final_.get_unchecked_mut(old_len + r_idx) = r;
          }
        }
      }
      final_ = new_final_;
    }
    Ok(SelectResult::new(&result_tbls, &final_))
  }
}