use chrono::NaiveDate;

use common::{*, Error::*, BareTy::*};
use syntax::ast::{*, CmpOp::*};
use physics::*;
use crate::is_null;

macro_rules! handle_op {
  ($cmp: ident, $op:expr, $p: ident, $l: expr, $r: expr) => {
    match $op {
      Lt => $cmp!(<, false, $p, $l, $r), Le => $cmp!(<, false, $p, $l, $r), Ge => $cmp!(<, false, $p, $l, $r),
      Gt => $cmp!(<, false, $p, $l, $r), Eq => $cmp!(<, false, $p, $l, $r), Ne => $cmp!(<, true, $p, $l, $r),
    }
  };
}

// assume both lhs and rhs belongs to tp's table, so ColRef::table is not checked
pub unsafe fn one_predicate(e: &Expr, tp: &TablePage) -> Result<Box<dyn Fn(*const u8) -> bool>> {
  let l = tp.p().r().get_ci(e.lhs_col().col)?;
  let l_idx = (l as *const ColInfo).offset_from(tp.cols.as_ptr()) as usize;
  let l_off = l.off as usize;
  match e {
    Expr::Cmp(op, _, r) => match r {
      &Atom::Lit(r) => {
        macro_rules! cmp {
          ($op: tt, $nullable: expr, $p: ident, $l: expr, $r: expr) => {
            Ok(Box::new(move |$p| {
              if is_null($p, l_idx) { return $nullable; }
              $l $op $r
            }))
          };
        }
        // the match logic is basically the same as the logic in `fill_ptr`, though the content is different
        match (l.ty.ty, r) {
          (_, Lit::Null) => Err(CmpOnNull),
          (Int, Lit::Int(v)) => handle_op!(cmp, op, p, *(p.add(l_off) as *const i32), v),
          (Bool, Lit::Bool(v)) => handle_op!(cmp, op, p, *(p.add(l_off) as *const bool), v),
          (Float, Lit::Float(v)) => handle_op!(cmp, op, p, *(p.add(l_off) as *const f32), v),
          (Char, Lit::Str(v)) | (VarChar, Lit::Str(v)) => {
            let v = Box::<str>::from(v);
            handle_op!(cmp, op, p, str_from_parts(p.add(l_off + 1), *p.add(l_off) as usize), &v)
          }
          (Date, Lit::Str(v)) => match NaiveDate::parse_from_str(v, "%Y-%m-%d") {
            Ok(date) => handle_op!(cmp, op, p, *(p.add(l_off) as *const NaiveDate), date),
            Err(reason) => return Err(InsertInvalidDate { date: (*v).into(), reason })
          }
          (expect, r)  => return Err(RecordLitTyMismatch { expect, actual: r.ty() })
        }
      }
      Atom::ColRef(r) => {
        let r = tp.p().r().get_ci(r.col)?;
        let r_idx = (r as *const ColInfo).offset_from(tp.cols.as_ptr()) as usize;
        let r_off = r.off as usize;
        macro_rules! cmp {
          ($op: tt, $nullable: expr, $p: ident, $l: expr, $r: expr) => {
            Ok(Box::new(move |$p| {
              if is_null($p, l_idx) { return $nullable; }
              if is_null($p, r_idx) { return $nullable; }
              $l $op $r
            }))
          };
        }
        match (l.ty.ty, r.ty.ty) {
          (Int, Int) => handle_op!(cmp, op, p, *(p.add(l_off) as *const i32), *(p.add(r_off) as *const i32)),
          (Bool, Bool) => handle_op!(cmp, op, p, *(p.add(l_off) as *const bool), *(p.add(r_off) as *const bool)),
          (Float, Float) => handle_op!(cmp, op, p, *(p.add(l_off) as *const f32), *(p.add(r_off) as *const f32)),
          (Char, Char) | (Char, VarChar) | (VarChar, Char) | (VarChar, VarChar) =>
            handle_op!(cmp, op, p, str_from_parts(p.add(l_off + 1), *p.add(l_off) as usize),
                str_from_parts(p.add(r_off + 1), *p.add(r_off) as usize)),
          (Date, Date) => handle_op!(cmp, op, p, *(p.add(l_off) as *const NaiveDate), *(p.add(r_off) as *const NaiveDate)),
          (expect, actual) => return Err(RecordTyMismatch { expect, actual })
        }
      }
    },
    Expr::Null(_, null) =>
      Ok(if *null { Box::new(move |p| is_null(p, l_idx)) } else { Box::new(move |p| !is_null(p, l_idx)) }),
    Expr::Like(_, pat) => {
      match l.ty.ty { Char | VarChar => {} ty => return Err(InvalidLikeTy(ty)) }
      let pat = regex::escape(pat).replace('%', ".*").replace('_', ".");
      match regex::Regex::new(&pat) {
        Ok(re) => Ok(Box::new(move |p|
          !is_null(p, l_idx) &&
            re.is_match(str_from_parts(p.add(l_off + 1), *p.add(l_off) as usize))
        )),
        Err(err) => Err(InvalidLike(err)),
      }
    }
  }
}

pub unsafe fn cross_predicate(op: CmpOp, col: (&str, &str), tp: (&TablePage, &TablePage)) -> Result<Box<dyn Fn((*const u8, *const u8)) -> bool>> {
  let l = tp.0.p().r().get_ci(col.0)?;
  let l_idx = (l as *const ColInfo).offset_from(tp.0.cols.as_ptr()) as usize;
  let l_off = l.off as usize;
  let r = tp.1.p().r().get_ci(col.1)?;
  let r_idx = (r as *const ColInfo).offset_from(tp.1.cols.as_ptr()) as usize;
  let r_off = r.off as usize;
  macro_rules! cmp {
    ($op: tt, $nullable: expr, $p: ident, $l: expr, $r: expr) => {
      Ok(Box::new(move |$p| {
        if is_null($p.0, l_idx) { return $nullable; }
        if is_null($p.1, r_idx) { return $nullable; }
        $l $op $r
      }))
    };
  }
  match (l.ty.ty, r.ty.ty) {
    (Int, Int) => handle_op!(cmp, op, p, *(p.0.add(l_off) as *const i32), *(p.1.add(r_off) as *const i32)),
    (Bool, Bool) => handle_op!(cmp, op, p, *(p.0.add(l_off) as *const bool), *(p.1.add(r_off) as *const bool)),
    (Float, Float) => handle_op!(cmp, op, p, *(p.0.add(l_off) as *const f32), *(p.1.add(r_off) as *const f32)),
    (Char, Char) | (Char, VarChar) | (VarChar, Char) | (VarChar, VarChar) =>
      handle_op!(cmp, op, p, str_from_parts(p.0.add(l_off + 1), *p.0.add(l_off) as usize),
                str_from_parts(p.1.add(r_off + 1), *p.1.add(r_off) as usize)),
    (Date, Date) => handle_op!(cmp, op, p, *(p.0.add(l_off) as *const NaiveDate), *(p.1.add(r_off) as *const NaiveDate)),
    (expect, actual) => return Err(RecordTyMismatch { expect, actual })
  }
}

#[inline]
pub fn and<T: Copy>(ps: Vec<Box<dyn Fn(T) -> bool>>) -> impl Fn(T) -> bool {
  move |t| ps.iter().all(|p| p(t))
}