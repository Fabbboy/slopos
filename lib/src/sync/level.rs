use core::marker::PhantomData;

pub trait Level: Sized + 'static {}

pub trait Lower<L: Level>: Level {}

pub struct L0(PhantomData<()>);
pub struct L1(PhantomData<()>);
pub struct L2(PhantomData<()>);
pub struct L3(PhantomData<()>);
pub struct L4(PhantomData<()>);
pub struct L5(PhantomData<()>);

impl Level for L0 {}
impl Level for L1 {}
impl Level for L2 {}
impl Level for L3 {}
impl Level for L4 {}
impl Level for L5 {}

impl Lower<L1> for L0 {}
impl Lower<L2> for L0 {}
impl Lower<L3> for L0 {}
impl Lower<L4> for L0 {}
impl Lower<L5> for L0 {}

impl Lower<L2> for L1 {}
impl Lower<L3> for L1 {}
impl Lower<L4> for L1 {}
impl Lower<L5> for L1 {}

impl Lower<L3> for L2 {}
impl Lower<L4> for L2 {}
impl Lower<L5> for L2 {}

impl Lower<L4> for L3 {}
impl Lower<L5> for L3 {}

impl Lower<L5> for L4 {}
