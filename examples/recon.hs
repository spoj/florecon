#!/usr/bin/env cabal
{- cabal:
build-depends:
    base >= 4.20 && < 4.23,
    containers >= 0.6 && < 0.9,
    relude >= 1.2 && < 2
default-language: GHC2024
ghc-options: -Wall
-}

{-# LANGUAGE GHC2024 #-}
{-# LANGUAGE NoImplicitPrelude #-}
{-# LANGUAGE OverloadedStrings #-}

-- florecon — a small, single-file Haskell port of core.py + recon.py
--
-- Run with:
--   cabal run examples/recon.hs
--
-- A Strategy is a pure function:
--
--   bag -> (resolved groups, residual bag)
--
-- Entries have stable identities and opaque payloads.  Input ids must be
-- unique; under that precondition, each valid strategy conserves identity:
-- every input id occurs in exactly one group or residual.

module Main where

import Relude

import qualified Data.List as List
import qualified Data.Map.Strict as Map
import qualified Data.Ord as Ord
import qualified Data.Set as Set
import qualified Data.Text as Text

-- Core model -----------------------------------------------------------------

type Id = Int

data Entry a = Entry
    { entryId :: Id
    , entryPayload :: a
    }
    deriving (Eq, Show)

data Group = Group
    { groupMembers :: [Id]
    , groupOrigin :: Text
    , groupReason :: [Text]
    }
    deriving (Eq, Show)

data Resolution a = Resolution
    { resolvedGroups :: [Group]
    , residual :: [Entry a]
    }
    deriving (Eq, Show)

newtype Strategy a = Strategy
    { runStrategy :: [Entry a] -> Resolution a
    }

type GroupView a = [Entry a]

net :: (Entry a -> Integer) -> GroupView a -> Integer
net amount = sum . fmap amount

gross :: (Entry a -> Integer) -> GroupView a -> Integer
gross amount = sum . fmap (abs . amount)

minSide :: (Entry a -> Integer) -> GroupView a -> Int
minSide amount members =
    min
        (length (filter ((> 0) . amount) members))
        (length (filter ((< 0) . amount) members))

split :: Text -> [[Id]] -> [Entry a] -> Resolution a
split origin matches bag =
    let used = Set.fromList (concat matches)
        groups = fmap (\ids -> Group ids origin []) matches
        leftovers = filter (\entry -> Set.notMember (entryId entry) used) bag
     in Resolution groups leftovers

residualFrom :: [Entry a] -> [Group] -> [Entry a]
residualFrom bag groups =
    let used = Set.fromList (concatMap groupMembers groups)
     in filter (\entry -> Set.notMember (entryId entry) used) bag

viewOf :: Map.Map Id (Entry a) -> Group -> GroupView a
viewOf source grp =
    mapMaybe (`Map.lookup` source) (groupMembers grp)

bucketBy :: Ord key => (value -> key) -> [value] -> Map.Map key [value]
bucketBy keyOf =
    List.foldl'
        (\buckets value ->
            Map.insertWith (flip (<>)) (keyOf value) [value] buckets
        )
        Map.empty

bucketMaybeBy :: Ord key => (value -> Maybe key) -> [value] -> Map.Map key [value]
bucketMaybeBy keyOf =
    List.foldl'
        (\buckets value -> case keyOf value of
            Nothing -> buckets
            Just key -> Map.insertWith (flip (<>)) key [value] buckets
        )
        Map.empty

-- Combinators ----------------------------------------------------------------

identityS :: Strategy a
identityS = Strategy (Resolution [])

-- | Cascade: each strategy receives only the previous strategy's residual.
cascade :: [Strategy a] -> Strategy a
cascade steps = Strategy $ \bag ->
    List.foldl' runStep (Resolution [] bag) steps
  where
    runStep current step =
        let next = runStrategy step (residual current)
         in Resolution
                (resolvedGroups current <> resolvedGroups next)
                (residual next)

explain :: Text -> Strategy a -> Strategy a
explain note inner = Strategy $ \bag ->
    let answer = runStrategy inner bag
        addNote grp = grp {groupReason = groupReason grp <> [note]}
     in answer {resolvedGroups = fmap addNote (resolvedGroups answer)}

-- | Run a strategy only on rows satisfying a predicate.
onlyWhen :: (Entry a -> Bool) -> Strategy a -> Strategy a
onlyWhen predicate inner = Strategy $ \bag ->
    let answer = runStrategy inner (filter predicate bag)
        groups = resolvedGroups answer
     in Resolution groups (residualFrom bag groups)

-- | Shard the bag by a key and build one strategy per shard.
partitionBy :: Ord key => (Entry a -> key) -> (key -> Strategy a) -> Strategy a
partitionBy keyOf factory = Strategy $ \bag ->
    let runShard (key, shard) = runStrategy (factory key) shard
        answers = fmap runShard (Map.toAscList (bucketBy keyOf bag))
        groups = concatMap resolvedGroups answers
     in Resolution groups (residualFrom bag groups)

-- | Rejecting a candidate dissolves the whole group back into residual.
acceptIf :: (GroupView a -> Bool) -> Strategy a -> Strategy a
acceptIf predicate inner = Strategy $ \bag ->
    let answer = runStrategy inner bag
        source = Map.fromList (fmap (\entry -> (entryId entry, entry)) bag)
        accepted = filter (predicate . viewOf source) (resolvedGroups answer)
     in Resolution accepted (residualFrom bag accepted)

soak :: Text -> Strategy a
soak origin = Strategy $ \bag ->
    if null bag
        then Resolution [] []
        else Resolution [Group (fmap entryId bag) origin []] []

-- | Repeat a strategy on its own residual until no ids change.
fixedPoint :: Int -> Strategy a -> Strategy a
fixedPoint maxPasses inner = Strategy (go (max 0 maxPasses) [])
  where
    fingerprint = List.sort . fmap entryId

    go passes groups bag
        | passes == 0 || null bag = Resolution groups bag
        | otherwise =
            let answer = runStrategy inner bag
                allGroups = groups <> resolvedGroups answer
                leftovers = residual answer
             in if fingerprint leftovers == fingerprint bag
                    then Resolution allGroups leftovers
                    else go (passes - 1) allGroups leftovers

-- Matching leaves ------------------------------------------------------------

-- | Pair equal-and-opposite rows sharing a key.  'Nothing' opts a row out.
exact1to1
    :: Ord key
    => (Entry a -> Maybe key)
    -> (Entry a -> Integer)
    -> Strategy a
exact1to1 keyOf amount = Strategy $ \bag ->
    let matches = concatMap matchShard (Map.elems (bucketMaybeBy keyOf bag))
     in split "exact_1to1" matches bag
  where
    matchShard shard =
        let ordered = List.sortOn entryId shard
            positives = amountsBySign (> 0) ordered
            negatives = amountsBySign (< 0) ordered
            pairMagnitude (magnitude, positiveIds) =
                zipWith
                    (\positiveId negativeId -> [positiveId, negativeId])
                    positiveIds
                    (Map.findWithDefault [] magnitude negatives)
         in concatMap pairMagnitude (Map.toAscList positives)

    amountsBySign sign =
        List.foldl'
            (\byMagnitude entry ->
                let value = amount entry
                 in if sign value
                        then
                            Map.insertWith
                                (flip (<>))
                                (abs value)
                                [entryId entry]
                                byMagnitude
                        else byMagnitude
            )
            Map.empty

-- | Bucket by key and retain each bucket accepted by the caller.
aggNet
    :: Ord key
    => (Entry a -> Maybe key)
    -> (GroupView a -> Bool)
    -> Strategy a
aggNet keyOf accept = Strategy $ \bag ->
    let matches =
            [ fmap entryId members
            | members <- Map.elems (bucketMaybeBy keyOf bag)
            , accept members
            ]
     in split "agg_net" matches bag

-- | Whole-lot subset clearing in both directions.  The largest free lot
-- anchors and draws at most @maxGroup - 1@ opposite-sign lots.  'band' only
-- widens the search; wrap this in 'acceptIf' for the final accounting rule.
subsetSum
    :: (Entry a -> Integer)
    -> Integer
    -> Int
    -> Strategy a
subsetSum amount band maxGroup = Strategy $ \bag ->
    let tolerance = max 0 band
        anchors =
            List.sortOn
                (\entry -> (Ord.Down (abs (amount entry)), entryId entry))
                (filter ((/= 0) . amount) bag)
        matches = findMatches tolerance anchors Set.empty [] bag
     in split "subset_sum" matches bag
  where
    findMatches _ [] _ found _ = reverse found
    findMatches tolerance (anchor : anchors) used found bag
        | Set.member (entryId anchor) used =
            findMatches tolerance anchors used found bag
        | otherwise =
            let anchorValue = amount anchor
                wantedSign = negate (signum anchorValue)
                pool =
                    List.sortOn
                        (\(ident, value) -> (Ord.Down value, ident))
                        [ (entryId entry, abs value)
                        | entry <- bag
                        , let value = amount entry
                        , Set.notMember (entryId entry) used
                        , entryId entry /= entryId anchor
                        , signum value == wantedSign
                        ]
             in case findSubset tolerance (maxGroup - 1) (abs anchorValue) pool of
                    Nothing -> findMatches tolerance anchors used found bag
                    Just picked ->
                        let memberIds = entryId anchor : picked
                            nowUsed = Set.union used (Set.fromList memberIds)
                         in findMatches tolerance anchors nowUsed (memberIds : found) bag

findSubset
    :: Integer
    -> Int
    -> Integer
    -> [(Id, Integer)]
    -> Maybe [Id]
findSubset tolerance maxPicks target = search maxPicks target []
  where
    search picks remaining chosen pool
        | abs remaining <= tolerance && not (null chosen) = Just (reverse chosen)
        | picks <= 0 || remaining < negate tolerance = Nothing
        | otherwise = choose picks remaining chosen pool

    choose _ _ _ [] = Nothing
    choose picks remaining chosen ((ident, value) : rest)
        | value > remaining + tolerance = choose picks remaining chosen rest
        | otherwise =
            case search (picks - 1) (remaining - value) (ident : chosen) rest of
                Just answer -> Just answer
                Nothing -> choose picks remaining chosen rest

-- Runner ---------------------------------------------------------------------

data Labeled a = Labeled
    { labeledId :: Id
    , labeledPayload :: a
    , labeledGroup :: Maybe Int
    , labeledOrigin :: Text
    , labeledReason :: Text
    }
    deriving (Eq, Show)

label :: Strategy a -> [Entry a] -> [Labeled a]
label strategy bag =
    let answer = runStrategy strategy bag
        assignments =
            Map.fromList
                [ ( ident
                  , (number, groupOrigin grp, Text.intercalate " / " (groupReason grp))
                  )
                | (number, grp) <- zip [0 ..] (resolvedGroups answer)
                , ident <- groupMembers grp
                ]
        labelOne entry =
            case Map.lookup (entryId entry) assignments of
                Nothing ->
                    Labeled (entryId entry) (entryPayload entry) Nothing "residual" ""
                Just (number, origin, reason) ->
                    Labeled (entryId entry) (entryPayload entry) (Just number) origin reason
     in fmap labelOne bag

-- Demo -----------------------------------------------------------------------

data Tx = Tx
    { txAmount :: Integer
    , txAccount :: Int
    }
    deriving (Eq, Show)

main :: IO ()
main = do
    let bag =
            zipWith
                Entry
                [1 ..]
                [ Tx 100 1
                , Tx (-100) 1
                , Tx 30 2
                , Tx 70 2
                , Tx (-100) 2
                , Tx 5 1
                ]
        amountOf = txAmount . entryPayload
        accountOf = Just . txAccount . entryPayload
        strategy =
            cascade
                [ exact1to1 accountOf amountOf
                , explain "same-account tolerance" $
                    aggNet accountOf (\members -> abs (net amountOf members) <= 5)
                , acceptIf
                    (\members -> net amountOf members == 0)
                    (subsetSum amountOf 0 4)
                ]

    traverse_ print (label strategy bag)
